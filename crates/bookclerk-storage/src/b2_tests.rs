//! [`B2Backend`] against an in-memory B2 that speaks the native API.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::{json, Value};
use sha1::Sha1;
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::*;
use crate::b2_api::{decode_file_name, encode_file_name};

/// The fake bucket's name.
const BUCKET: &str = "bk-test";

/// The key id the fake accepts.
const KEY_ID: &str = "key-id";

/// The application key the fake accepts.
const APP_KEY: &str = "app-key";

/// One stored file version.
#[derive(Clone)]
struct Stored {
    /// File id.
    id: String,
    /// File name.
    name: String,
    /// Bytes.
    data: Vec<u8>,
    /// Content type.
    content_type: String,
    /// File info.
    info: BTreeMap<String, String>,
    /// Upload time (a counter).
    ts: u64,
    /// `upload`, `copy` or `hide`.
    action: &'static str,
}

/// An unfinished large file.
struct Large {
    /// File name.
    name: String,
    /// Content type.
    content_type: String,
    /// File info.
    info: BTreeMap<String, String>,
    /// Parts by number.
    parts: BTreeMap<u32, Vec<u8>>,
}

/// The fake's state.
#[derive(Default)]
struct World {
    /// Base URL, set once listening.
    base: String,
    /// The current account token.
    token: String,
    /// Sign-ins so far.
    sign_ins: u32,
    /// Versions, oldest first.
    files: Vec<Stored>,
    /// Unfinished large files by id.
    large: HashMap<String, Large>,
    /// Ids handed out.
    next_id: u64,
    /// Make the next API call answer 401 `expired_auth_token`.
    expire_next: bool,
    /// Make this many uploads answer 503.
    busy_uploads: u32,
    /// Restrict the key to this bucket name, when set.
    restrict_to: Option<String>,
    /// Large files cancelled.
    cancelled: u32,
    /// Make the next `b2_finish_large_file` commit, then answer 503 as if
    /// its reply were lost.
    lose_finish_reply: bool,
    /// Ids `b2_delete_file_version` removed, in order.
    deleted: Vec<String>,
}

impl World {
    /// A fresh id.
    fn id(&mut self) -> String {
        self.next_id += 1;
        format!("4_z{:04}", self.next_id)
    }

    /// The JSON B2 gives for a version.
    fn file_json(file: &Stored) -> Value {
        json!({
            "fileId": file.id,
            "fileName": file.name,
            "contentLength": file.data.len(),
            "contentSha1": hex::encode(Sha1::digest(&file.data)),
            "contentType": file.content_type,
            "uploadTimestamp": file.ts,
            "action": file.action,
            "fileInfo": file.info,
        })
    }

    /// The newest version of `name`, when it is not hidden.
    fn live(&self, name: &str) -> Option<&Stored> {
        self.files
            .iter()
            .rev()
            .find(|file| file.name == name)
            .filter(|file| file.action != "hide")
    }
}

/// A reply: status, extra headers, body.
type Reply = (u16, Vec<(String, String)>, Vec<u8>);

/// A JSON reply.
fn reply_json(status: u16, value: &Value) -> Reply {
    (status, Vec::new(), value.to_string().into_bytes())
}

/// An error reply.
fn reply_error(status: u16, code: &str) -> Reply {
    reply_json(
        status,
        &json!({ "status": status, "code": code, "message": code }),
    )
}

/// Answers one request.
fn handle(
    world: &mut World,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Reply {
    let auth = headers.get("authorization").cloned().unwrap_or_default();
    if path == "/b2api/v3/b2_authorize_account" {
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{KEY_ID}:{APP_KEY}"))
        );
        if auth != expected {
            return reply_error(401, "bad_auth_token");
        }
        world.sign_ins += 1;
        world.token = format!("token-{}", world.sign_ins);
        let (bucket_id, bucket_name) = match &world.restrict_to {
            Some(name) => (json!("b1"), json!(name)),
            None => (Value::Null, Value::Null),
        };
        return reply_json(
            200,
            &json!({
                "accountId": "acct",
                "authorizationToken": world.token,
                "apiInfo": { "storageApi": {
                    "apiUrl": world.base,
                    "downloadUrl": world.base,
                    "recommendedPartSize": 100,
                    "absoluteMinimumPartSize": 5,
                    "bucketId": bucket_id,
                    "bucketName": bucket_name,
                    "namePrefix": Value::Null,
                    "capabilities": ["listFiles"],
                }},
            }),
        );
    }
    if let Some(op) = path.strip_prefix("/b2api/v3/") {
        if world.expire_next {
            world.expire_next = false;
            return reply_error(401, "expired_auth_token");
        }
        if auth != world.token {
            return reply_error(401, "bad_auth_token");
        }
        let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        return api(world, op, &request);
    }
    if path == "/upload/file" || path.starts_with("/upload/part/") {
        if auth != "upload-token" {
            return reply_error(401, "bad_auth_token");
        }
        if world.busy_uploads > 0 {
            world.busy_uploads -= 1;
            return reply_error(503, "service_unavailable");
        }
        if hex::encode(Sha1::digest(body))
            != headers
                .get("x-bz-content-sha1")
                .cloned()
                .unwrap_or_default()
        {
            return reply_error(400, "bad_request");
        }
        if let Some(file_id) = path.strip_prefix("/upload/part/") {
            let number: u32 = headers
                .get("x-bz-part-number")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            let Some(large) = world.large.get_mut(file_id) else {
                return reply_error(400, "bad_request");
            };
            large.parts.insert(number, body.to_vec());
            return reply_json(
                200,
                &json!({ "partNumber": number, "contentSha1": hex::encode(Sha1::digest(body)) }),
            );
        }
        let name = decode_file_name(&headers["x-bz-file-name"]).unwrap_or_default();
        let info = headers
            .iter()
            .filter_map(|(key, value)| {
                key.strip_prefix("x-bz-info-")
                    .map(|key| (key.to_string(), decode_file_name(value).unwrap_or_default()))
            })
            .collect();
        let stored = Stored {
            id: world.id(),
            name,
            data: body.to_vec(),
            content_type: headers["content-type"].clone(),
            info,
            ts: world.next_id,
            action: "upload",
        };
        world.files.push(stored.clone());
        return reply_json(200, &World::file_json(&stored));
    }
    if let Some(rest) = path.strip_prefix(&format!("/file/{BUCKET}/")) {
        if auth != world.token {
            return reply_error(401, "bad_auth_token");
        }
        let name = decode_file_name(rest).unwrap_or_default();
        let Some(file) = world.live(&name) else {
            return reply_error(404, "not_found");
        };
        let size = file.data.len();
        let mut extra = vec![
            ("X-Bz-File-Id".to_string(), file.id.clone()),
            ("X-Bz-File-Name".to_string(), encode_file_name(&file.name)),
            (
                "X-Bz-Content-Sha1".to_string(),
                hex::encode(Sha1::digest(&file.data)),
            ),
            ("X-Bz-Upload-Timestamp".to_string(), file.ts.to_string()),
            ("Content-Type".to_string(), file.content_type.clone()),
        ];
        for (key, value) in &file.info {
            extra.push((format!("X-Bz-Info-{key}"), encode_file_name(value)));
        }
        let (status, body) = match headers.get("range") {
            None => (200, file.data.clone()),
            Some(range) => {
                let span = range.strip_prefix("bytes=").unwrap_or_default();
                let (start, end) = span.split_once('-').unwrap_or_default();
                let start: usize = start.parse().unwrap_or(0);
                let end: usize = if end.is_empty() {
                    size.saturating_sub(1)
                } else {
                    end.parse().unwrap_or(0)
                };
                if start >= size {
                    return reply_error(416, "range_not_satisfiable");
                }
                let end = end.min(size - 1);
                extra.push((
                    "Content-Range".to_string(),
                    format!("bytes {start}-{end}/{size}"),
                ));
                (206, file.data[start..=end].to_vec())
            }
        };
        extra.push(("Content-Length".to_string(), body.len().to_string()));
        let body = if method == "HEAD" { Vec::new() } else { body };
        return (status, extra, body);
    }
    reply_error(404, "not_found")
}

/// Answers one API operation.
fn api(world: &mut World, op: &str, request: &Value) -> Reply {
    let text = |key: &str| request[key].as_str().unwrap_or_default().to_string();
    match op {
        "b2_list_buckets" => reply_json(
            200,
            &json!({ "buckets": [{ "bucketId": "b1", "bucketName": BUCKET }] }),
        ),
        "b2_get_upload_url" => reply_json(
            200,
            &json!({ "uploadUrl": format!("{}/upload/file", world.base), "authorizationToken": "upload-token" }),
        ),
        "b2_get_upload_part_url" => reply_json(
            200,
            &json!({
                "uploadUrl": format!("{}/upload/part/{}", world.base, text("fileId")),
                "authorizationToken": "upload-token",
            }),
        ),
        "b2_start_large_file" => {
            let id = world.id();
            let info: BTreeMap<String, String> =
                serde_json::from_value(request["fileInfo"].clone()).unwrap_or_default();
            world.large.insert(
                id.clone(),
                Large {
                    name: text("fileName"),
                    content_type: text("contentType"),
                    info: info.clone(),
                    parts: BTreeMap::new(),
                },
            );
            reply_json(
                200,
                &json!({ "fileId": id, "fileName": text("fileName"), "action": "start", "fileInfo": info }),
            )
        }
        "b2_finish_large_file" => {
            let id = text("fileId");
            let sha1s: Vec<String> =
                serde_json::from_value(request["partSha1Array"].clone()).unwrap_or_default();
            let Some(large) = world.large.get(&id) else {
                return reply_error(400, "bad_request");
            };
            // Part numbers run 1..=n and each SHA-1 matches its part, in
            // order; a refused finish leaves the upload to retry or cancel.
            let stored_sha1s: Vec<String> = large
                .parts
                .values()
                .map(|part| hex::encode(Sha1::digest(part)))
                .collect();
            let numbered = large
                .parts
                .keys()
                .copied()
                .eq(1..=u32::try_from(large.parts.len()).unwrap_or(0));
            if large.parts.len() < 2 || !numbered || sha1s != stored_sha1s {
                return reply_error(400, "bad_request");
            }
            let Some(large) = world.large.remove(&id) else {
                return reply_error(400, "bad_request");
            };
            let data: Vec<u8> = large.parts.values().flatten().copied().collect();
            let stored = Stored {
                id,
                name: large.name,
                data,
                content_type: large.content_type,
                info: large.info,
                ts: world.next_id,
                action: "upload",
            };
            world.files.push(stored.clone());
            if world.lose_finish_reply {
                world.lose_finish_reply = false;
                return reply_error(503, "service_unavailable");
            }
            let mut reply = World::file_json(&stored);
            reply["contentSha1"] = json!("none");
            reply_json(200, &reply)
        }
        "b2_get_file_info" => {
            let id = text("fileId");
            if let Some(file) = world.files.iter().find(|file| file.id == id) {
                let mut reply = World::file_json(file);
                reply["contentSha1"] = json!("none");
                return reply_json(200, &reply);
            }
            match world.large.get(&id) {
                Some(large) => reply_json(
                    200,
                    &json!({ "fileId": id, "fileName": large.name, "action": "start" }),
                ),
                None => reply_error(404, "not_found"),
            }
        }
        "b2_cancel_large_file" => {
            world.cancelled += 1;
            match world.large.remove(&text("fileId")) {
                Some(_) => reply_json(200, &json!({})),
                None => reply_error(400, "bad_request"),
            }
        }
        "b2_list_file_names" => {
            let prefix = text("prefix");
            let start = text("startFileName");
            let max =
                usize::try_from(request["maxFileCount"].as_u64().unwrap_or(100)).unwrap_or(100);
            let mut names: Vec<&Stored> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut sorted: Vec<&Stored> = world.files.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name).then(b.ts.cmp(&a.ts)));
            for file in sorted {
                if !file.name.starts_with(&prefix) || file.name < start || !seen.insert(&file.name)
                {
                    continue;
                }
                if file.action != "hide" {
                    names.push(file);
                }
            }
            let next = names.get(max).map(|file| file.name.clone());
            let files: Vec<Value> = names
                .iter()
                .take(max)
                .map(|f| World::file_json(f))
                .collect();
            reply_json(200, &json!({ "files": files, "nextFileName": next }))
        }
        "b2_list_file_versions" => {
            let prefix = text("prefix");
            let start = text("startFileName");
            let mut sorted: Vec<&Stored> = world
                .files
                .iter()
                .filter(|file| file.name.starts_with(&prefix) && file.name >= start)
                .collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name).then(b.ts.cmp(&a.ts)));
            let mut files: Vec<Value> = sorted.iter().map(|f| World::file_json(f)).collect();
            for (id, large) in &world.large {
                if large.name.starts_with(&prefix) {
                    files.push(json!({ "fileId": id, "fileName": large.name, "action": "start" }));
                }
            }
            reply_json(200, &json!({ "files": files }))
        }
        "b2_delete_file_version" => {
            let id = text("fileId");
            let before = world.files.len();
            world.files.retain(|file| file.id != id);
            if world.files.len() == before {
                return reply_error(400, "file_not_present");
            }
            world.deleted.push(id.clone());
            reply_json(200, &json!({ "fileId": id, "fileName": text("fileName") }))
        }
        "b2_copy_file" => {
            let source = text("sourceFileId");
            let Some(found) = world.files.iter().find(|file| file.id == source).cloned() else {
                return reply_error(404, "not_found");
            };
            let stored = Stored {
                id: world.id(),
                name: text("fileName"),
                ts: world.next_id,
                action: "copy",
                ..found
            };
            world.files.push(stored.clone());
            reply_json(200, &World::file_json(&stored))
        }
        _ => reply_error(400, "bad_request"),
    }
}

/// Serves the fake and returns it with its base URL.
async fn serve() -> Arc<Mutex<World>> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let world = Arc::new(Mutex::new(World {
        base,
        ..World::default()
    }));
    let shared = Arc::clone(&world);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let world = Arc::clone(&shared);
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
                let request = lines.next().unwrap_or_default().to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or("/").to_string();
                let length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = collected[header_end..].to_vec();
                while body.len() < length {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..n]);
                }
                let (status, extra, reply) = {
                    let mut world = world.lock().await;
                    handle(&mut world, &method, &path, &headers, &body)
                };
                let mut out = format!("HTTP/1.1 {status} X\r\nConnection: close\r\n");
                if !extra.iter().any(|(k, _)| k == "Content-Length") {
                    out.push_str(&format!("Content-Length: {}\r\n", reply.len()));
                }
                for (key, value) in &extra {
                    out.push_str(&format!("{key}: {value}\r\n"));
                }
                out.push_str("\r\n");
                let _ = socket.write_all(out.as_bytes()).await;
                let _ = socket.write_all(&reply).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    world
}

/// Opens a backend on the fake with 8-byte parts.
async fn open(world: &Arc<Mutex<World>>, prefix: &str) -> B2Backend {
    let base = world.lock().await.base.clone();
    let mut settings = B2Settings::new(KEY_ID, B2ApplicationKey::new(APP_KEY), BUCKET, prefix);
    settings.authorize_url = base;
    B2Backend::with_part_floor(settings, 8).expect("open")
}

/// A body over `bytes`.
fn body(bytes: &[u8]) -> Pin<Box<dyn AsyncRead + Send>> {
    Box::pin(std::io::Cursor::new(bytes.to_vec()))
}

/// Reads a whole stream.
async fn drain(mut reader: Pin<Box<dyn AsyncRead + Send>>) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).await?;
    Ok(out)
}

#[tokio::test]
async fn small_files_round_trip_with_metadata() {
    let world = serve().await;
    let backend = open(&world, "books").await;
    assert_eq!(backend.name(), "b2");
    assert_eq!(backend.instance_id(), "b2:bk-test/books/");
    assert!(backend.supports_server_copy());
    assert!(!backend.exists("a/b.m4b").await.expect("exists"));

    let meta = ObjectMeta {
        content_type: Some("audio/mp4".into()),
        content_length: Some(5),
        asin: Some("B00TEST".into()),
        title: Some("Ünïcode: a title".into()),
        commit_token: Some("tok".into()),
        ..ObjectMeta::default()
    };
    let put = backend
        .put_stream("a/b c.m4b", body(b"hello"), meta)
        .await
        .expect("put");
    assert_eq!(put.bytes_written, 5);
    assert_eq!(
        put.sha256_hex.as_deref(),
        Some(hex::encode(Sha256::digest(b"hello")).as_str())
    );
    assert_eq!(
        world.lock().await.files[0].name,
        "books/a/b c.m4b",
        "keys live under the prefix"
    );

    let probe = backend.probe("a/b c.m4b").await.expect("probe");
    assert_eq!(probe.size, 5);
    assert_eq!(probe.content_type.as_deref(), Some("audio/mp4"));
    assert_eq!(probe.meta.asin.as_deref(), Some("B00TEST"));
    assert_eq!(probe.meta.title.as_deref(), Some("Ünïcode: a title"));
    assert_eq!(probe.meta.commit_token.as_deref(), Some("tok"));
    assert_eq!(probe.meta.sha256_hex, put.sha256_hex);
    assert_eq!(probe.etag, put.etag);
    assert_eq!(backend.get("a/b c.m4b").await.expect("get"), "hello");

    let (ranged, reader) = backend
        .get_stream(
            "a/b c.m4b",
            Some(ByteRange {
                offset: 1,
                length: Some(3),
            }),
        )
        .await
        .expect("range");
    assert_eq!(ranged.size, 5, "the probe gives the whole size");
    assert_eq!(drain(reader).await.expect("read"), b"ell");
    let (_, tail) = backend
        .get_stream(
            "a/b c.m4b",
            Some(ByteRange {
                offset: 5,
                length: None,
            }),
        )
        .await
        .expect("empty tail");
    assert!(drain(tail).await.expect("read").is_empty());
    assert!(matches!(
        backend
            .get_stream(
                "a/b c.m4b",
                Some(ByteRange {
                    offset: 6,
                    length: None
                })
            )
            .await,
        Err(StorageError::InvalidKey(_))
    ));
    assert!(matches!(
        backend.probe("missing").await,
        Err(StorageError::NotFound(_))
    ));
    assert!(matches!(
        backend.get_stream("missing", None).await,
        Err(StorageError::NotFound(_))
    ));
}

#[tokio::test]
async fn long_bodies_become_large_files() {
    let world = serve().await;
    let backend = open(&world, "").await;
    let data: Vec<u8> = (0..30u8).collect();
    let put = backend
        .put_stream(
            "big.m4b",
            body(&data),
            ObjectMeta {
                content_length: Some(30),
                ..ObjectMeta::default()
            },
        )
        .await
        .expect("put");
    assert_eq!(put.bytes_written, 30);
    {
        let world = world.lock().await;
        assert!(world.large.is_empty(), "the large file was finished");
        assert_eq!(world.files[0].data, data);
    }
    let (_, reader) = backend.get_stream("big.m4b", None).await.expect("get");
    assert_eq!(drain(reader).await.expect("read"), data);

    // Exactly one part is a small file: B2 needs two parts for a large one.
    backend
        .put_stream("eight", body(&data[..8]), ObjectMeta::default())
        .await
        .expect("put one part");
    assert_eq!(world.lock().await.files.len(), 2);
}

#[tokio::test]
async fn a_finish_whose_reply_was_lost_is_read_back() {
    let world = serve().await;
    let backend = open(&world, "").await;
    world.lock().await.lose_finish_reply = true;
    let data: Vec<u8> = (0..20u8).collect();
    let put = backend
        .put_stream("big", body(&data), ObjectMeta::default())
        .await
        .expect("the finished file is found by id");
    assert_eq!(put.bytes_written, 20);
    let world = world.lock().await;
    assert_eq!(world.cancelled, 0, "a finished file is not cancelled");
    assert_eq!(world.files.len(), 1);
    assert_eq!(world.files[0].data, data);
}

#[tokio::test]
async fn the_sign_in_url_must_be_https() {
    let mut settings = B2Settings::new(KEY_ID, B2ApplicationKey::new(APP_KEY), BUCKET, "");
    settings.authorize_url = "http://api.backblazeb2.com".into();
    assert!(matches!(
        B2Backend::new(settings),
        Err(StorageError::InvalidKey(_))
    ));
}

#[tokio::test]
async fn failed_large_uploads_are_cancelled() {
    let world = serve().await;
    let backend = open(&world, "").await;
    let data = vec![7u8; 20];
    let err = backend
        .put_stream(
            "big",
            body(&data),
            ObjectMeta {
                sha256_hex: Some(hex::encode(Sha256::digest(b"other"))),
                ..ObjectMeta::default()
            },
        )
        .await
        .expect_err("digest mismatch");
    assert!(matches!(err, StorageError::Integrity(_)), "{err}");
    let world = world.lock().await;
    assert_eq!(world.cancelled, 1);
    assert!(world.large.is_empty() && world.files.is_empty());
}

#[tokio::test]
async fn declared_lengths_and_digests_are_checked() {
    let world = serve().await;
    let backend = open(&world, "").await;
    let long = backend
        .put_stream(
            "a",
            body(b"abc"),
            ObjectMeta {
                content_length: Some(2),
                ..ObjectMeta::default()
            },
        )
        .await;
    assert!(matches!(long, Err(StorageError::Integrity(_))));
    let short = backend
        .put_stream(
            "a",
            body(b"abc"),
            ObjectMeta {
                content_length: Some(4),
                ..ObjectMeta::default()
            },
        )
        .await;
    assert!(matches!(short, Err(StorageError::Integrity(_))));
    let digest = backend
        .put_stream(
            "a",
            body(b"abc"),
            ObjectMeta {
                sha256_hex: Some(hex::encode(Sha256::digest(b"abd"))),
                ..ObjectMeta::default()
            },
        )
        .await;
    assert!(matches!(digest, Err(StorageError::Integrity(_))));
    assert!(world.lock().await.files.is_empty(), "nothing was stored");
    assert!(matches!(
        backend
            .put("/bad", Bytes::from_static(b"x"), ObjectMeta::default())
            .await,
        Err(StorageError::InvalidKey(_))
    ));
}

#[tokio::test]
async fn expired_tokens_sign_in_again_and_busy_uploads_retry() {
    let world = serve().await;
    let backend = open(&world, "").await;
    assert!(!backend.exists("a").await.expect("signs in"));
    {
        let mut world = world.lock().await;
        world.expire_next = true;
        world.busy_uploads = 2;
    }
    backend
        .put("a", Bytes::from_static(b"abc"), ObjectMeta::default())
        .await
        .expect("put after retries");
    assert_eq!(world.lock().await.sign_ins, 2);
    // A download with a stale token signs in again too.
    world.lock().await.token = "rotated".into();
    assert_eq!(backend.get("a").await.expect("get"), "abc");
    assert_eq!(world.lock().await.sign_ins, 3);
    // Busy past every retry is an error.
    world.lock().await.busy_uploads = 10;
    assert!(matches!(
        backend
            .put("b", Bytes::from_static(b"x"), ObjectMeta::default())
            .await,
        Err(StorageError::Remote(_))
    ));
}

#[tokio::test]
async fn listings_page_under_the_prefix() {
    let world = serve().await;
    let backend = open(&world, "lib").await;
    for key in ["a/1", "a/2", "a/3", "b/1"] {
        backend
            .put(key, Bytes::from_static(b"x"), ObjectMeta::default())
            .await
            .expect("put");
    }
    let first = backend.list_page("a/", None, 2).await.expect("page");
    let keys: Vec<_> = first.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a/1", "a/2"]);
    let cursor = first.next_cursor.expect("more");
    let second = backend
        .list_page("a/", Some(&cursor), 2)
        .await
        .expect("page");
    let keys: Vec<_> = second.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a/3"]);
    assert!(second.next_cursor.is_none());
    assert!(matches!(
        backend.list_page("a/", Some("elsewhere/x"), 2).await,
        Err(StorageError::InvalidCursor(_))
    ));
    let all = backend.list_page("", None, 0).await.expect("all");
    assert_eq!(all.objects.len(), 4);

    // Without a prefix the cursor still moves the listing on.
    let bare = open(&world, "").await;
    let first = bare.list_page("", None, 3).await.expect("page");
    let cursor = first.next_cursor.expect("more");
    let rest = bare.list_page("", Some(&cursor), 3).await.expect("page");
    let keys: Vec<_> = rest.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["lib/b/1"]);
}

#[tokio::test]
async fn copies_are_server_side_and_deletes_remove_every_version() {
    let world = serve().await;
    let backend = open(&world, "").await;
    backend
        .put("a", Bytes::from_static(b"one"), ObjectMeta::default())
        .await
        .expect("put");
    backend
        .put("a", Bytes::from_static(b"two!"), ObjectMeta::default())
        .await
        .expect("put again");
    assert_eq!(backend.copy_sized("a", "b").await.expect("copy"), 4);
    assert_eq!(backend.get("b").await.expect("get"), "two!");
    assert!(matches!(
        backend.copy("missing", "c").await,
        Err(StorageError::NotFound(_))
    ));

    let ids: Vec<String> = world
        .lock()
        .await
        .files
        .iter()
        .filter(|file| file.name == "a")
        .map(|file| file.id.clone())
        .collect();
    backend.delete("a").await.expect("delete");
    assert!(!backend.exists("a").await.expect("exists"));
    {
        let world = world.lock().await;
        assert!(
            world.files.iter().all(|file| file.name != "a"),
            "both versions are gone"
        );
        assert_eq!(world.deleted, ids, "oldest first, current version last");
    }
    backend.delete("a").await.expect("deleting nothing is fine");
    assert!(backend.exists("b").await.expect("exists"));
}

#[tokio::test]
async fn deletes_cancel_unfinished_large_files() {
    let world = serve().await;
    let backend = open(&world, "").await;
    world.lock().await.large.insert(
        "4_zlarge".into(),
        Large {
            name: "a".into(),
            content_type: AUTO_CONTENT_TYPE.into(),
            info: BTreeMap::new(),
            parts: BTreeMap::new(),
        },
    );
    backend.delete("a").await.expect("delete");
    assert!(world.lock().await.large.is_empty());
}

#[tokio::test]
async fn keys_restricted_elsewhere_are_refused() {
    let world = serve().await;
    world.lock().await.restrict_to = Some("other-bucket".into());
    let base = world.lock().await.base.clone();
    let settings = |secret: &str| {
        let mut settings = B2Settings::new(KEY_ID, B2ApplicationKey::new(secret), BUCKET, "");
        settings.authorize_url = base.clone();
        B2Backend::new(settings).expect("settings")
    };
    assert!(matches!(
        settings(APP_KEY).exists("a").await,
        Err(StorageError::InvalidKey(_))
    ));
    world.lock().await.restrict_to = Some(BUCKET.into());
    assert!(!settings(APP_KEY)
        .exists("a")
        .await
        .expect("restricted to this bucket"));
    assert!(matches!(
        settings("wrong").exists("a").await,
        Err(StorageError::Remote(_))
    ));
    assert!(matches!(
        B2Backend::new(B2Settings::new(
            "",
            B2ApplicationKey::new(APP_KEY),
            BUCKET,
            ""
        )),
        Err(StorageError::InvalidKey(_))
    ));
    assert!(matches!(
        B2Backend::new(B2Settings::new(
            KEY_ID,
            B2ApplicationKey::new(APP_KEY),
            " ",
            ""
        )),
        Err(StorageError::InvalidKey(_))
    ));
    assert_eq!(
        format!("{:?}", B2ApplicationKey::new("secret")),
        "B2ApplicationKey([redacted])"
    );
}

#[tokio::test]
async fn keys_translate_like_other_object_stores() {
    let world = serve().await;
    let backend = open(&world, "p").await;
    let key = crate::key_syntax::HostKey::parse("Author/Title.m4b").expect("key");
    assert_eq!(
        backend.translate_keys(&[key]).await.expect("translate"),
        ["Author/Title.m4b"]
    );
    assert!(backend.capabilities().translate_keys);
    assert!(!backend.capabilities().conditional_write);
}
