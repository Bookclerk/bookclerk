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
    /// Rows per `b2_list_file_versions` page, when set: the fake then
    /// pages with `nextFileName` / `nextFileId`, and may return fewer rows
    /// than `maxFileCount`, as B2 does.
    versions_page: Option<usize>,
    /// The `range` of each `b2_copy_part`, in order.
    copy_ranges: Vec<String>,
    /// Make the next `b2_upload_file` land just after another writer's
    /// upload of the same name, as if the two raced.
    racer: bool,
    /// The bucket's event notification rules, as B2 lists them.
    rules: Vec<Value>,
    /// `b2_set_bucket_notification_rules` calls so far.
    rule_sets: u32,
    /// List rules without their signing secret and batch size, which B2
    /// does not document repeating.
    terse_rules: bool,
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

    /// The rules as a reply lists them.
    fn listed_rules(&self) -> Vec<Value> {
        self.rules
            .iter()
            .map(|rule| {
                let mut rule = rule.clone();
                if self.terse_rules {
                    if let Some(target) = rule["targetConfiguration"].as_object_mut() {
                        target.remove("hmacSha256SigningSecret");
                        target.remove("maxEventsPerBatch");
                    }
                }
                rule
            })
            .collect()
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
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let api_op = path
        .strip_prefix("/b2api/v3/")
        .or_else(|| path.strip_prefix("/b2api/v4/"));
    if let Some(op) = api_op {
        // B2 documents the rules listing as a GET with a query string.
        let get_only = op == "b2_get_bucket_notification_rules";
        if get_only != (method == "GET") {
            return reply_error(405, "method_not_allowed");
        }
        if world.expire_next {
            world.expire_next = false;
            return reply_error(401, "expired_auth_token");
        }
        if auth != world.token {
            return reply_error(401, "bad_auth_token");
        }
        let request: Value = if get_only {
            let fields: serde_json::Map<String, Value> = query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| (key.to_string(), json!(value)))
                .collect();
            Value::Object(fields)
        } else {
            serde_json::from_slice(body).unwrap_or(Value::Null)
        };
        if get_only && request["bucketId"].as_str().unwrap_or_default().is_empty() {
            return reply_error(400, "bad_request");
        }
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
        if std::mem::take(&mut world.racer) {
            let racer = Stored {
                id: world.id(),
                name: name.clone(),
                data: b"racer".to_vec(),
                content_type: "text/plain".into(),
                info: BTreeMap::new(),
                ts: world.next_id,
                action: "upload",
            };
            world.files.push(racer);
        }
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
        "b2_get_bucket_notification_rules" => reply_json(
            200,
            &json!({ "bucketId": text("bucketId"), "eventNotificationRules": world.listed_rules() }),
        ),
        "b2_set_bucket_notification_rules" => {
            let Some(rules) = request["eventNotificationRules"].as_array() else {
                return reply_error(400, "bad_request");
            };
            // A suspended rule can only be deleted, not sent again.
            let resends_suspended = rules.iter().any(|rule| {
                world
                    .rules
                    .iter()
                    .any(|old| old["name"] == rule["name"] && old["isSuspended"] == json!(true))
            });
            if resends_suspended || rules.len() > 25 {
                return reply_error(400, "bad_request");
            }
            world.rule_sets += 1;
            world.rules = rules
                .iter()
                .map(|rule| {
                    let mut rule = rule.clone();
                    rule["isSuspended"] = json!(false);
                    rule["suspensionReason"] = json!("");
                    rule
                })
                .collect();
            reply_json(
                200,
                &json!({ "bucketId": text("bucketId"), "eventNotificationRules": world.listed_rules() }),
            )
        }
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
        "b2_copy_part" => {
            let range = text("range");
            world.copy_ranges.push(range.clone());
            let Some(source) = world
                .files
                .iter()
                .find(|file| file.id == text("sourceFileId"))
                .cloned()
            else {
                return reply_error(400, "bad_request");
            };
            let Some((start, end)) = range
                .strip_prefix("bytes=")
                .and_then(|span| span.split_once('-'))
                .and_then(|(start, end)| {
                    Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
                })
                .filter(|&(start, end)| start <= end && end < source.data.len())
            else {
                return reply_error(400, "bad_request");
            };
            let number = u32::try_from(request["partNumber"].as_u64().unwrap_or(0)).unwrap_or(0);
            let bytes = source.data[start..=end].to_vec();
            let sha1 = hex::encode(Sha1::digest(&bytes));
            let Some(large) = world.large.get_mut(&text("largeFileId")) else {
                return reply_error(400, "bad_request");
            };
            let length = bytes.len();
            large.parts.insert(number, bytes);
            reply_json(
                200,
                &json!({ "fileId": text("largeFileId"), "partNumber": number, "contentLength": length, "contentSha1": sha1 }),
            )
        }
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
            if let Some(size) = world.versions_page {
                let start_id = text("startFileId");
                let from = sorted
                    .iter()
                    .position(|file| file.id == start_id)
                    .unwrap_or(0);
                let rest = &sorted[from..];
                let page: Vec<Value> = rest
                    .iter()
                    .take(size)
                    .map(|f| World::file_json(f))
                    .collect();
                let mut reply = json!({ "files": page });
                if let Some(next) = rest.get(size) {
                    reply["nextFileName"] = json!(next.name);
                    reply["nextFileId"] = json!(next.id);
                }
                return reply_json(200, &reply);
            }
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
async fn a_name_with_too_many_versions_is_refused_before_any_delete() {
    let world = serve().await;
    let backend = open(&world, "").await;
    {
        let mut world = world.lock().await;
        // Short pages: ten pages of 999 rows leave 9,990 versions with a
        // continuation, and the last page brings the walk to 10,001
        // without one.
        world.versions_page = Some(999);
        for _ in 0..10_001 {
            let id = world.id();
            let ts = world.next_id;
            world.files.push(Stored {
                id,
                name: "many".into(),
                data: Vec::new(),
                content_type: "application/octet-stream".into(),
                info: BTreeMap::new(),
                ts,
                action: "upload",
            });
        }
    }
    let err = backend.delete("many").await.expect_err("too many versions");
    assert!(
        err.to_string().contains("more than 10000 versions"),
        "{err}"
    );
    let world = world.lock().await;
    assert!(world.deleted.is_empty());
    assert_eq!(world.files.len(), 10_001);
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
    assert!(backend.capabilities().checked_write);
}

/// Writes `data` to `key` under `condition`.
async fn put_if(
    backend: &B2Backend,
    key: &str,
    data: &'static [u8],
    condition: &WriteCondition,
) -> Result<crate::ConditionalPutResult> {
    backend
        .put_stream_conditional(key, body(data), ObjectMeta::default(), condition)
        .await
}

#[tokio::test]
async fn a_write_that_must_be_new_is_withdrawn_when_the_name_exists() {
    let world = serve().await;
    let backend = open(&world, "").await;
    let first = put_if(&backend, "a", b"one", &WriteCondition::IfNoneMatch)
        .await
        .expect("a new name takes the write");
    assert!(!first.atomic, "a checked write is never atomic");
    let refused = put_if(&backend, "a", b"two", &WriteCondition::IfNoneMatch).await;
    assert!(
        matches!(refused, Err(StorageError::Conflict(_))),
        "{refused:?}"
    );
    assert_eq!(backend.get("a").await.expect("get"), "one");
    {
        let world = world.lock().await;
        assert_eq!(world.files.len(), 1, "the losing write was withdrawn");
        assert_eq!(world.deleted.len(), 1);
    }

    // A hidden name counts as absent.
    let hide_id = {
        let mut world = world.lock().await;
        let id = world.id();
        let ts = world.next_id;
        world.files.push(Stored {
            id: id.clone(),
            name: "a".into(),
            data: Vec::new(),
            content_type: String::new(),
            info: BTreeMap::new(),
            ts,
            action: "hide",
        });
        id
    };
    put_if(&backend, "a", b"three", &WriteCondition::IfNoneMatch)
        .await
        .expect("a hidden name takes the write");
    assert_eq!(backend.get("a").await.expect("get"), "three");
    assert!(world.lock().await.files.iter().any(|f| f.id == hide_id));
}

#[tokio::test]
async fn a_write_that_must_replace_a_version_checks_which_it_replaced() {
    let world = serve().await;
    let backend = open(&world, "").await;
    backend
        .put("a", Bytes::from_static(b"one"), ObjectMeta::default())
        .await
        .expect("put");
    let current = backend.probe("a").await.expect("probe").etag.expect("etag");
    let stale = put_if(
        &backend,
        "a",
        b"two",
        &WriteCondition::IfMatch("4_zstale".into()),
    )
    .await;
    assert!(matches!(stale, Err(StorageError::Conflict(_))), "{stale:?}");
    assert_eq!(backend.get("a").await.expect("get"), "one");
    put_if(&backend, "a", b"three", &WriteCondition::IfMatch(current))
        .await
        .expect("the expected version was replaced");
    assert_eq!(backend.get("a").await.expect("get"), "three");
    let missing = put_if(
        &backend,
        "b",
        b"four",
        &WriteCondition::IfMatch("4_z0001".into()),
    )
    .await;
    assert!(
        matches!(missing, Err(StorageError::Conflict(_))),
        "{missing:?}"
    );
    assert!(!backend.exists("b").await.expect("exists"));
}

#[tokio::test]
async fn of_two_racing_writes_the_one_listed_first_stands() {
    let world = serve().await;
    let backend = open(&world, "").await;
    world.lock().await.racer = true;
    let lost = put_if(&backend, "a", b"ours", &WriteCondition::IfNoneMatch).await;
    assert!(matches!(lost, Err(StorageError::Conflict(_))), "{lost:?}");
    assert_eq!(
        backend.get("a").await.expect("get"),
        "racer",
        "the earlier write stands and ours is gone"
    );
    assert_eq!(world.lock().await.files.len(), 1);
}

#[test]
fn file_info_keeps_the_digest_and_token_and_drops_display_fields_that_do_not_fit() {
    let meta = ObjectMeta {
        asin: Some("B000000001".into()),
        title: Some("t".repeat(1900)),
        commit_token: Some("0123456789abcdef0123456789abcdef".into()),
        ..ObjectMeta::default()
    };
    let digest = "0".repeat(64);
    let info = file_info("books/a.m4b", &meta, Some(&digest));
    assert_eq!(info.get("sha256"), Some(&digest));
    assert!(info.contains_key("commit-token"));
    assert!(info.contains_key("asin"));
    // The title alone fits B2's limit but not beside the name and the
    // required fields.
    assert!(!info.contains_key("title"));
    let used = "X-Bz-File-Name".len()
        + encode_file_name("books/a.m4b").len()
        + info
            .iter()
            .map(|(key, value)| "X-Bz-Info-".len() + key.len() + encode_file_name(value).len())
            .sum::<usize>();
    assert!(used <= MAX_NAME_AND_INFO_BYTES);

    let short = ObjectMeta {
        title: Some("A short title".into()),
        ..ObjectMeta::default()
    };
    assert!(file_info("a", &short, None).contains_key("title"));
}

#[test]
fn file_info_counts_every_header_name_up_to_the_exact_limit() {
    // Name "a": 14 for `X-Bz-File-Name` plus 1. Digest: 10 for
    // `X-Bz-Info-`, 6 for `sha256`, 64 hex. Title: 10 + 5 + its length.
    let digest = "0".repeat(64);
    let room = MAX_NAME_AND_INFO_BYTES - (14 + 1) - (10 + 6 + 64) - (10 + 5);
    let exact = ObjectMeta {
        title: Some("t".repeat(room)),
        ..ObjectMeta::default()
    };
    assert!(file_info("a", &exact, Some(&digest)).contains_key("title"));
    let over = ObjectMeta {
        title: Some("t".repeat(room + 1)),
        ..ObjectMeta::default()
    };
    assert!(!file_info("a", &over, Some(&digest)).contains_key("title"));
}

#[tokio::test]
async fn a_large_copy_goes_part_by_part() {
    let world = serve().await;
    let backend = open(&world, "").await;
    // Eleven bytes: past this build's one-call copy limit (10), in parts
    // of four with a short last part.
    backend
        .put(
            "big",
            Bytes::from_static(b"hello world"),
            ObjectMeta::default(),
        )
        .await
        .expect("put");
    assert_eq!(backend.copy_sized("big", "copy").await.expect("copy"), 11);
    assert_eq!(backend.get("copy").await.expect("get"), "hello world");
    let world = world.lock().await;
    assert_eq!(world.copy_ranges, ["bytes=0-3", "bytes=4-7", "bytes=8-10"]);
    assert!(world.large.is_empty());
}

/// The webhook binding's side of the feed tests.
#[derive(Default)]
struct HookState {
    /// The URL `url` answers.
    url: Option<String>,
    /// Requests waiting: lease id when leased, body.
    waiting: Vec<Vec<u8>>,
    /// Leased requests by lease id.
    leased: Vec<(String, Vec<u8>)>,
    /// Lease ids acknowledged, in order.
    acked: Vec<String>,
    /// Lease ids handed out so far.
    leases: u64,
    /// What the next pull reports as missed.
    missed: bool,
}

/// An in-memory [`Webhooks`] binding.
struct FakeHooks(std::rc::Rc<std::cell::RefCell<HookState>>);

#[async_trait::async_trait(?Send)]
impl bookclerk_plugin_sdk::Webhooks for FakeHooks {
    async fn url(
        &self,
        name: String,
    ) -> std::result::Result<Option<String>, bookclerk_plugin_sdk::PluginError> {
        assert_eq!(name, "changes");
        Ok(self.0.borrow().url.clone())
    }

    async fn pull(
        &self,
        request: bookclerk_plugin_sdk::WebhookPullRequest,
    ) -> std::result::Result<bookclerk_plugin_sdk::WebhookPullPage, bookclerk_plugin_sdk::PluginError>
    {
        assert!(request.batch_size >= 1);
        assert!(request.visibility_timeout_ms <= bookclerk_plugin_sdk::MAX_WEBHOOK_VISIBILITY_MS);
        let mut state = self.0.borrow_mut();
        let take = state.waiting.len().min(request.batch_size as usize);
        let bodies: Vec<Vec<u8>> = state.waiting.drain(..take).collect();
        let mut deliveries = Vec::new();
        for body in bodies {
            state.leases += 1;
            let lease_id = format!("lease-{}", state.leases);
            state.leased.push((lease_id.clone(), body.clone()));
            deliveries.push(bookclerk_plugin_sdk::WebhookDelivery {
                lease_id,
                body,
                attempt: 1,
                ..Default::default()
            });
        }
        Ok(bookclerk_plugin_sdk::WebhookPullPage {
            deliveries,
            missed: std::mem::take(&mut state.missed),
        })
    }

    async fn ack(
        &self,
        request: bookclerk_plugin_sdk::WebhookAckRequest,
    ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
        let mut state = self.0.borrow_mut();
        for lease in &request.acks {
            state.leased.retain(|(id, _)| id != lease);
            state.acked.push(lease.clone());
        }
        for lease in &request.retries {
            if let Some(at) = state.leased.iter().position(|(id, _)| id == lease) {
                let (_, body) = state.leased.remove(at);
                state.waiting.push(body);
            }
        }
        Ok(())
    }
}

/// The URL the fake binding hands out.
const HOOK_URL: &str = "https://library.example/hooks/0123456789abcdef/changes";

/// A signing secret.
fn hook_secret() -> crate::b2_events::B2SigningSecret {
    crate::b2_events::B2SigningSecret::parse("abcdefghijklmnopqrstuvwxyz012345").expect("secret")
}

/// A rule that is not the destination's.
fn foreign_rule() -> Value {
    json!({
        "name": "someone-elses",
        "eventTypes": ["b2:ObjectCreated:*"],
        "isEnabled": true,
        "objectNamePrefix": "other/",
        "targetConfiguration": { "targetType": "webhook", "url": "https://elsewhere.example/hook" },
        "isSuspended": false,
        "suspensionReason": "",
    })
}

/// A webhook body with one event per `(type, bucket, name)`.
fn event_body(events: &[(&str, &str, &str)]) -> Vec<u8> {
    let events: Vec<Value> = events
        .iter()
        .enumerate()
        .map(|(at, (kind, bucket, name))| {
            json!({
                "eventType": kind,
                "bucketName": bucket,
                "objectName": name,
                "eventTimestamp": 1_700_000_000_000u64 + at as u64,
            })
        })
        .collect();
    json!({ "events": events }).to_string().into_bytes()
}

/// A feed over the fake bucket under `library/`, with its state in `dir`.
async fn open_feed(
    world: &Arc<Mutex<World>>,
    hooks: &std::rc::Rc<std::cell::RefCell<HookState>>,
    secret: Option<crate::b2_events::B2SigningSecret>,
    dir: &std::path::Path,
) -> crate::B2ChangeFeed {
    let backend = open(world, "library").await;
    crate::B2ChangeFeed::open(
        backend,
        Some(Box::new(FakeHooks(std::rc::Rc::clone(hooks)))),
        "changes",
        secret,
        dir,
    )
    .expect("feed")
}

/// The names of the bucket's rules.
fn rule_names_of(world: &World) -> Vec<String> {
    world
        .rules
        .iter()
        .map(|rule| rule["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn the_feed_writes_its_rules_and_hands_out_events_under_its_prefix() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    world.lock().await.rules.push(foreign_rule());
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    assert!(feed.is_on());

    let start = feed
        .changes(None, 0)
        .await
        .expect("start")
        .expect("feed on");
    assert!(!start.complete, "a new feed starts with a listing");
    {
        let world = world.lock().await;
        let mut expected = vec!["someone-elses".to_string()];
        expected.extend(crate::b2_events::rule_names("library/"));
        assert_eq!(rule_names_of(&world), expected);
        assert_eq!(
            world.rules[0]["targetConfiguration"],
            foreign_rule()["targetConfiguration"]
        );
        let own = &world.rules[1];
        assert_eq!(own["objectNamePrefix"], "library/");
        assert_eq!(own["eventTypes"], json!(["b2:ObjectCreated:*"]));
        assert_eq!(own["targetConfiguration"]["url"], HOOK_URL);
        assert_eq!(
            own["targetConfiguration"]["hmacSha256SigningSecret"],
            hook_secret().expose()
        );
        assert_eq!(world.rule_sets, 1);
    }

    hooks.borrow_mut().waiting.push(event_body(&[
        ("b2:ObjectCreated:Upload", BUCKET, "library/a.m4b"),
        ("b2:ObjectCreated:Upload", "another", "library/b.m4b"),
        ("b2:HideMarkerCreated:Hide", BUCKET, "other/c.m4b"),
        ("b2:ObjectDeleted:Delete", BUCKET, "library/d.m4b"),
    ]));
    let page = feed
        .changes(Some(&start.cursor), 0)
        .await
        .expect("page")
        .expect("feed on");
    let keys: Vec<&str> = page.changes.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, ["a.m4b", "d.m4b"]);
    assert!(page
        .changes
        .iter()
        .all(|c| c.kind == crate::ObjectChangeKind::Changed));
    assert!(page.complete, "rules in place and nothing missed");
    assert!(
        hooks.borrow().acked.is_empty(),
        "acked only after the next call"
    );

    let after = feed
        .changes(Some(&page.cursor), 0)
        .await
        .expect("after")
        .expect("feed on");
    assert!(after.changes.is_empty());
    assert_eq!(hooks.borrow().acked, ["lease-1"]);
    assert_eq!(world.lock().await.rule_sets, 1, "rules are not rewritten");

    // Requests the host dropped make the page incomplete.
    hooks.borrow_mut().missed = true;
    let gap = feed
        .changes(Some(&after.cursor), 0)
        .await
        .expect("gap")
        .expect("feed on");
    assert!(!gap.complete);
}

#[tokio::test]
async fn the_listing_cursor_starts_the_feed_over() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    let page = feed
        .changes(Some(crate::guest::NO_FEED_CURSOR), 0)
        .await
        .expect("page")
        .expect("feed on");
    assert!(!page.complete);
    assert!(page.cursor.starts_with("q1."));
}

#[tokio::test]
async fn suspended_rules_are_deleted_then_written_again() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    {
        let mut world = world.lock().await;
        world.rules.push(foreign_rule());
        for mut rule in crate::b2_events::own_rules("library/", HOOK_URL, &hook_secret()) {
            rule["isSuspended"] = json!(true);
            rule["suspensionReason"] = json!("too many failed deliveries");
            world.rules.push(rule);
        }
    }
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    let page = feed.changes(None, 0).await.expect("page").expect("feed on");
    assert!(!page.complete);
    let world = world.lock().await;
    assert_eq!(world.rule_sets, 2, "deleted in one call, added in the next");
    assert_eq!(world.rules.len(), 4);
    assert!(world
        .rules
        .iter()
        .all(|rule| rule["isSuspended"] == json!(false)));
}

#[tokio::test]
async fn a_changed_url_rewrites_the_rules_and_the_page_is_incomplete() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    let start = feed
        .changes(None, 0)
        .await
        .expect("start")
        .expect("feed on");
    let steady = feed
        .changes(Some(&start.cursor), 0)
        .await
        .expect("steady")
        .expect("feed on");
    assert!(steady.complete);

    let moved = "https://moved.example/hooks/0123456789abcdef/changes";
    hooks.borrow_mut().url = Some(moved.into());
    let page = feed
        .changes(Some(&steady.cursor), 0)
        .await
        .expect("page")
        .expect("feed on");
    assert!(!page.complete, "events before the new rules went elsewhere");
    let world = world.lock().await;
    assert_eq!(world.rule_sets, 3, "removed, then added with the new URL");
    assert!(world
        .rules
        .iter()
        .all(|rule| rule["targetConfiguration"]["url"] == moved));
}

#[tokio::test]
async fn a_refused_rule_change_falls_back_to_listing() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    {
        // B2 holds at most 25 rules; the destination's three do not fit.
        let mut world = world.lock().await;
        for at in 0..24 {
            let mut rule = foreign_rule();
            rule["name"] = json!(format!("someone-elses-{at}"));
            world.rules.push(rule);
        }
    }
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    let start = feed
        .changes(None, 0)
        .await
        .expect("start")
        .expect("feed on");
    let page = feed
        .changes(Some(&start.cursor), 0)
        .await
        .expect("page")
        .expect("feed on");
    assert!(!page.complete, "without rules every page lists");
    assert_eq!(world.lock().await.rules.len(), 24, "other rules untouched");
}

#[tokio::test]
async fn without_a_url_or_a_secret_the_feed_removes_its_rules() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    world.lock().await.rules.push(foreign_rule());
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    feed.changes(None, 0)
        .await
        .expect("start")
        .expect("feed on");
    assert_eq!(world.lock().await.rules.len(), 4);

    // The library stops being reachable: no URL, no feed, no rules.
    hooks.borrow_mut().url = None;
    assert!(feed.changes(None, 0).await.expect("off").is_none());
    assert_eq!(rule_names_of(&*world.lock().await), ["someone-elses"]);

    // Back on, then the operator clears the secret.
    hooks.borrow_mut().url = Some(HOOK_URL.into());
    feed.changes(None, 0).await.expect("on").expect("feed on");
    assert_eq!(world.lock().await.rules.len(), 4);
    let off = open_feed(&world, &hooks, None, dir.path()).await;
    assert!(!off.is_on());
    assert!(off.changes(None, 0).await.expect("off").is_none());
    assert_eq!(rule_names_of(&*world.lock().await), ["someone-elses"]);
    let sets = world.lock().await.rule_sets;
    assert!(off.changes(None, 0).await.expect("off").is_none());
    assert_eq!(world.lock().await.rule_sets, sets, "removed once");
}

#[tokio::test]
async fn rules_listed_without_their_secret_still_count_as_in_place() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    world.lock().await.terse_rules = true;
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    let start = feed
        .changes(None, 0)
        .await
        .expect("start")
        .expect("feed on");
    assert_eq!(world.lock().await.rule_sets, 1, "written and verified once");
    // A fresh state file forces a check against B2's listing.
    for entry in std::fs::read_dir(dir.path()).expect("dir") {
        let path = entry.expect("entry").path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("b2-rules-"))
        {
            std::fs::remove_file(path).expect("remove");
        }
    }
    let page = feed
        .changes(Some(&start.cursor), 0)
        .await
        .expect("page")
        .expect("feed on");
    assert!(page.complete);
    assert_eq!(
        world.lock().await.rule_sets,
        1,
        "found in place, not rewritten"
    );
}

#[tokio::test]
async fn out_of_date_rules_are_deleted_before_they_are_written_again() {
    use crate::s3_guest::ChangeSource;
    let world = serve().await;
    world.lock().await.rules.extend(crate::b2_events::own_rules(
        "library/",
        "https://old.example/hook",
        &hook_secret(),
    ));
    let dir = tempfile::tempdir().expect("dir");
    let hooks = std::rc::Rc::new(std::cell::RefCell::new(HookState {
        url: Some(HOOK_URL.into()),
        ..HookState::default()
    }));
    let feed = open_feed(&world, &hooks, Some(hook_secret()), dir.path()).await;
    feed.changes(None, 0).await.expect("page").expect("feed on");
    let world = world.lock().await;
    assert_eq!(world.rule_sets, 2, "removed, then added with the new URL");
    assert!(world
        .rules
        .iter()
        .all(|rule| rule["targetConfiguration"]["url"] == HOOK_URL));
}
