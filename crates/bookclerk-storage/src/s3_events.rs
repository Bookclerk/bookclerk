//! Object-store event notifications, parsed into change-feed entries.
//!
//! An S3 bucket can announce writes and deletes on a notification queue.
//! The `s3` destination drains that queue to serve `changes`
//! (see `S3Backend::with_notification_queue`). The queue's messages
//! come from outside Bookclerk, so this module is the boundary that reads
//! them: a pure function over the message body, with no I/O, fuzzed by the
//! `s3_event` target.
//!
//! Three message shapes are read:
//!
//! - an S3 event notification (`{"Records": [...]}`), sent by S3 straight to
//!   the queue (MinIO sends the same shape);
//! - the same notification wrapped in an SNS envelope
//!   (`{"Type": "Notification", "Message": "<json>"}`), when the bucket
//!   notifies a topic the queue subscribes to without raw delivery;
//! - an EventBridge event (`{"source": "aws.s3", "detail-type": ...}`), when
//!   an EventBridge rule targets the queue.
//!
//! The shape is the first of `Records`, `Event`, `Type` and `source` that
//! the message carries (not null) at its top level. S3's own test message
//! (`"Event": "s3:TestEvent"`) yields no changes.
//! Records for another bucket, or for keys outside the destination prefix,
//! are skipped, so one queue can serve several buckets. Anything else is an
//! error: the caller counts the message as a change it could not read.

use serde::Deserialize;

use crate::{ObjectChange, ObjectChangeKind, ObjectInfo};

/// Largest message body read, in bytes. SQS caps a message at 1 MiB.
pub const MAX_EVENT_MESSAGE_BYTES: usize = 1024 * 1024;

/// Most records read from one message. S3 sends one record per message;
/// the cap bounds what a crafted message can make the feed hold.
pub const MAX_EVENT_RECORDS: usize = 256;

/// Longest object key accepted, in bytes after decoding (S3's own limit).
pub const MAX_EVENT_KEY_BYTES: usize = 1024;

/// Which objects a destination owns: the bucket and the key prefix that
/// `S3Backend` prepends to every key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventScope<'a> {
    /// Bucket name; records for other buckets are skipped.
    pub bucket: &'a str,
    /// Normalized key prefix (empty, or ending in `/`); it is stripped from
    /// every key, and keys outside it are skipped.
    pub prefix: &'a str,
}

/// One change read from a notification, with what orders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEvent {
    /// The change, keyed relative to the destination prefix.
    pub change: ObjectChange,
    /// The event time as the message states it (ISO 8601, UTC), when given.
    /// Times in one message format compare as text.
    pub time: Option<String>,
    /// S3's per-key sequencer (hex), when given: of two events for one key,
    /// the one with the greater sequencer happened later.
    pub sequencer: Option<String>,
}

/// Why a notification message could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventParseError {
    /// The body is over [`MAX_EVENT_MESSAGE_BYTES`].
    #[error("notification message is over {MAX_EVENT_MESSAGE_BYTES} bytes")]
    TooLarge,
    /// The body is not JSON of any known shape.
    #[error("notification message is not an S3, SNS or EventBridge event: {0}")]
    Malformed(String),
    /// The message holds more than [`MAX_EVENT_RECORDS`] records.
    #[error("notification message has more than {MAX_EVENT_RECORDS} records")]
    TooManyRecords,
    /// A record's key is not valid percent-encoded UTF-8, holds a NUL, or is
    /// too long.
    #[error("notification record has an unreadable key")]
    BadKey,
}

/// An S3 event notification.
#[derive(Deserialize)]
struct Notification {
    /// The records, one per event.
    #[serde(rename = "Records")]
    records: Vec<Record>,
}

/// An SNS envelope.
#[derive(Deserialize)]
struct SnsEnvelope {
    /// The wrapped message, as JSON text.
    #[serde(rename = "Message")]
    message: String,
}

/// An EventBridge event.
#[derive(Deserialize)]
struct BridgeEvent {
    /// What happened (`Object Created`, `Object Deleted`, ...).
    #[serde(rename = "detail-type")]
    detail_type: String,
    /// When it happened.
    #[serde(default)]
    time: Option<String>,
    /// The bucket and object.
    detail: BridgeDetail,
}

/// One record of an S3 event notification.
#[derive(Deserialize)]
struct Record {
    /// What happened, such as `ObjectCreated:Put` (MinIO prefixes `s3:`).
    #[serde(rename = "eventName")]
    event_name: String,
    /// When it happened.
    #[serde(rename = "eventTime", default)]
    event_time: Option<String>,
    /// The bucket and object.
    s3: RecordS3,
}

/// The `s3` member of a record.
#[derive(Deserialize)]
struct RecordS3 {
    /// The bucket.
    bucket: RecordBucket,
    /// The object.
    object: RecordObject,
}

/// A bucket named by a record.
#[derive(Deserialize)]
struct RecordBucket {
    /// Bucket name.
    name: String,
}

/// An object named by a record.
#[derive(Deserialize)]
struct RecordObject {
    /// Object key, percent-encoded with `+` for a space.
    key: String,
    /// Size in bytes, for created objects.
    #[serde(default)]
    size: Option<u64>,
    /// Entity tag without quotes, for created objects.
    #[serde(rename = "eTag", default)]
    etag: Option<String>,
    /// Version id, in a versioned bucket.
    #[serde(rename = "versionId", default)]
    version_id: Option<String>,
    /// Per-key ordering token.
    #[serde(default)]
    sequencer: Option<String>,
}

/// The `detail` member of an EventBridge event.
#[derive(Deserialize)]
struct BridgeDetail {
    /// For `Object Deleted`: `Delete Marker Created` or `Permanently Deleted`.
    #[serde(rename = "deletion-type", default)]
    deletion_type: Option<String>,
    /// The bucket.
    bucket: RecordBucket,
    /// The object.
    object: BridgeObject,
}

/// An object named by an EventBridge event.
#[derive(Deserialize)]
struct BridgeObject {
    /// Object key, not encoded.
    key: String,
    /// Size in bytes, for created objects.
    #[serde(default)]
    size: Option<u64>,
    /// Entity tag without quotes, for created objects.
    #[serde(default)]
    etag: Option<String>,
    /// Version id, in a versioned bucket.
    #[serde(rename = "version-id", default)]
    version_id: Option<String>,
    /// Per-key ordering token.
    #[serde(default)]
    sequencer: Option<String>,
}

/// What an event means for the feed.
enum Effect {
    /// The object now exists with these fields.
    Upserted,
    /// The object is gone.
    Removed,
    /// Something changed; the host probes the key.
    Changed,
    /// Nothing the feed reports (tags, ACLs, storage class).
    Ignored,
}

/// Reads one notification message into changes for `scope`, in the order
/// the message lists them.
///
/// # Arguments
///
/// * `body` - the message body as the queue delivered it.
/// * `scope` - the destination's bucket and key prefix.
///
/// # Returns
///
/// The changes for objects under `scope`; empty for a test message or a
/// message about other buckets or keys.
///
/// # Errors
///
/// Returns an [`EventParseError`] when the body is too large, is not one of
/// the known shapes, holds too many records, or names an unreadable key.
pub fn parse_event_message(
    body: &[u8],
    scope: EventScope<'_>,
) -> Result<Vec<ParsedEvent>, EventParseError> {
    parse_at_depth(body, scope, 0)
}

/// [`parse_event_message`], unwrapping at most one SNS envelope.
fn parse_at_depth(
    body: &[u8],
    scope: EventScope<'_>,
    depth: u8,
) -> Result<Vec<ParsedEvent>, EventParseError> {
    if body.len() > MAX_EVENT_MESSAGE_BYTES {
        return Err(EventParseError::TooLarge);
    }
    let malformed = |err: serde_json::Error| EventParseError::Malformed(err.to_string());
    // The whole message is read as a JSON tree first, so it is checked in
    // full (UTF-8, escapes) even where its members are ignored, and a
    // repeated member counts once, as its last value.
    let value: serde_json::Value = serde_json::from_slice(body).map_err(malformed)?;
    let Some(object) = value.as_object() else {
        return Err(EventParseError::Malformed("not a JSON object".into()));
    };
    let member = |name: &str| object.get(name).filter(|member| !member.is_null());
    if member("Records").is_some() {
        let notification = Notification::deserialize(&value).map_err(malformed)?;
        return records_events(notification.records, scope);
    }
    if let Some(event) = member("Event") {
        return if event.as_str() == Some("s3:TestEvent") {
            Ok(Vec::new())
        } else {
            Err(EventParseError::Malformed("unknown S3 message".into()))
        };
    }
    if let Some(kind) = member("Type") {
        if kind.as_str() != Some("Notification") || depth > 0 {
            return Err(EventParseError::Malformed("unexpected SNS message".into()));
        }
        let envelope = SnsEnvelope::deserialize(&value).map_err(malformed)?;
        return parse_at_depth(envelope.message.as_bytes(), scope, depth + 1);
    }
    match member("source").and_then(serde_json::Value::as_str) {
        Some("aws.s3") => {
            let event = BridgeEvent::deserialize(&value).map_err(malformed)?;
            bridge_events(event, scope)
        }
        _ => Err(EventParseError::Malformed("not an S3 event".into())),
    }
}

/// The changes in an S3 event notification's records.
fn records_events(
    records: Vec<Record>,
    scope: EventScope<'_>,
) -> Result<Vec<ParsedEvent>, EventParseError> {
    if records.len() > MAX_EVENT_RECORDS {
        return Err(EventParseError::TooManyRecords);
    }
    let mut out = Vec::new();
    for record in records {
        if record.s3.bucket.name != scope.bucket {
            continue;
        }
        let key = decode_form_key(&record.s3.object.key)?;
        let effect = record_effect(&record.event_name, &record.s3.object);
        if let Some(event) = event_for(
            key,
            effect,
            scope,
            ObjectFields {
                size: record.s3.object.size,
                etag: record.s3.object.etag,
                version_id: record.s3.object.version_id,
            },
            record.event_time,
            record.s3.object.sequencer,
        )? {
            out.push(event);
        }
    }
    Ok(out)
}

/// The change in an EventBridge event, if it is one the feed reports.
fn bridge_events(
    event: BridgeEvent,
    scope: EventScope<'_>,
) -> Result<Vec<ParsedEvent>, EventParseError> {
    let detail = event.detail;
    if detail.bucket.name != scope.bucket {
        return Ok(Vec::new());
    }
    if detail.object.key.len() > MAX_EVENT_KEY_BYTES || detail.object.key.contains('\0') {
        return Err(EventParseError::BadKey);
    }
    let effect = bridge_effect(
        &event.detail_type,
        detail.deletion_type.as_deref(),
        detail.object.version_id.as_deref(),
    );
    Ok(event_for(
        detail.object.key,
        effect,
        scope,
        ObjectFields {
            size: detail.object.size,
            etag: detail.object.etag,
            version_id: detail.object.version_id,
        },
        event.time,
        detail.object.sequencer,
    )?
    .into_iter()
    .collect())
}

/// The listing fields an event carries about its object.
struct ObjectFields {
    /// Size in bytes.
    size: Option<u64>,
    /// Entity tag without quotes.
    etag: Option<String>,
    /// Version id.
    version_id: Option<String>,
}

/// The change for one event, or `None` when the key is outside the scope
/// or the event is one the feed ignores.
fn event_for(
    full_key: String,
    effect: Effect,
    scope: EventScope<'_>,
    fields: ObjectFields,
    time: Option<String>,
    sequencer: Option<String>,
) -> Result<Option<ParsedEvent>, EventParseError> {
    let Some(key) = full_key.strip_prefix(scope.prefix) else {
        return Ok(None);
    };
    if key.is_empty() {
        return Ok(None);
    }
    let (kind, info) = match effect {
        Effect::Ignored => return Ok(None),
        Effect::Removed => (ObjectChangeKind::Removed, None),
        Effect::Changed => (ObjectChangeKind::Changed, None),
        Effect::Upserted => {
            let etag = fields.etag.filter(|e| !e.is_empty()).map(|e| quoted(&e));
            let info = fields.size.map(|size| ObjectInfo {
                key: key.to_string(),
                size,
                signature: etag.clone(),
                etag,
                modified_at_ms: None,
                version_id: fields.version_id.filter(|v| !v.is_empty()),
            });
            (ObjectChangeKind::Upserted, info)
        }
    };
    Ok(Some(ParsedEvent {
        change: ObjectChange {
            key: key.to_string(),
            kind,
            info,
        },
        time,
        sequencer,
    }))
}

/// The ETag as S3 lists it: in double quotes. Notifications leave them off.
fn quoted(etag: &str) -> String {
    if etag.len() >= 2 && etag.starts_with('"') && etag.ends_with('"') {
        etag.to_string()
    } else {
        format!("\"{etag}\"")
    }
}

/// What an S3 notification's `eventName` means.
///
/// Deleting one version of a versioned object may leave another current,
/// so that is a `Changed`, not a `Removed`.
fn record_effect(event_name: &str, object: &RecordObject) -> Effect {
    let name = event_name.strip_prefix("s3:").unwrap_or(event_name);
    let (family, action) = name.split_once(':').unwrap_or((name, ""));
    match family {
        "ObjectCreated" => Effect::Upserted,
        "ObjectRemoved" if action == "DeleteMarkerCreated" => Effect::Removed,
        "ObjectRemoved" if object.version_id.as_deref().is_some_and(|v| !v.is_empty()) => {
            Effect::Changed
        }
        "ObjectRemoved" | "LifecycleExpiration" => Effect::Removed,
        "ObjectRestore" | "ObjectReplicated" | "Replication" | "ObjectAccelerated" => {
            Effect::Changed
        }
        "ObjectTagging"
        | "ObjectAcl"
        | "LifecycleTransition"
        | "IntelligentTiering"
        | "ObjectAccessed"
        | "s3" => Effect::Ignored,
        _ => Effect::Changed,
    }
}

/// What an EventBridge `detail-type` means.
///
/// As with notifications, permanently deleting one version of a versioned
/// object may leave another current, so that is a `Changed`.
fn bridge_effect(
    detail_type: &str,
    deletion_type: Option<&str>,
    version_id: Option<&str>,
) -> Effect {
    match detail_type {
        "Object Created" => Effect::Upserted,
        "Object Deleted"
            if deletion_type == Some("Permanently Deleted")
                && version_id.is_some_and(|v| !v.is_empty()) =>
        {
            Effect::Changed
        }
        "Object Deleted" => Effect::Removed,
        "Object Tags Added"
        | "Object Tags Deleted"
        | "Object ACL Updated"
        | "Object Storage Class Changed"
        | "Object Access Tier Changed"
        | "Object Restore Initiated"
        | "Object Restore Expired" => Effect::Ignored,
        _ => Effect::Changed,
    }
}

/// Decodes an S3 notification key: `+` is a space and `%XX` a byte, and the
/// bytes must be UTF-8.
///
/// # Errors
///
/// Returns [`EventParseError::BadKey`] for a broken escape, bytes that are
/// not UTF-8, a NUL byte (which no storage key may hold), or a key over
/// [`MAX_EVENT_KEY_BYTES`].
fn decode_form_key(encoded: &str) -> Result<String, EventParseError> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hi = bytes.get(i + 1).copied().and_then(hex_value);
                let lo = bytes.get(i + 2).copied().and_then(hex_value);
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(EventParseError::BadKey);
                };
                out.push((hi << 4) | lo);
                i += 3;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    if out.len() > MAX_EVENT_KEY_BYTES || out.contains(&0) {
        return Err(EventParseError::BadKey);
    }
    String::from_utf8(out).map_err(|_| EventParseError::BadKey)
}

/// The value of one hex digit.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: EventScope<'static> = EventScope {
        bucket: "books",
        prefix: "library/",
    };

    fn record(name: &str, bucket: &str, key: &str) -> String {
        format!(
            r#"{{"eventVersion":"2.1","eventSource":"aws:s3","eventTime":"2026-11-18T08:00:00.000Z","eventName":"{name}","s3":{{"bucket":{{"name":"{bucket}"}},"object":{{"key":"{key}","size":12,"eTag":"abc","sequencer":"0055"}}}}}}"#
        )
    }

    fn records(items: &[String]) -> Vec<u8> {
        format!(r#"{{"Records":[{}]}}"#, items.join(",")).into_bytes()
    }

    #[test]
    fn a_put_is_an_upsert_with_the_listed_fields() {
        let body = records(&[record(
            "ObjectCreated:Put",
            "books",
            "library/A+B/c%C3%A9.m4b",
        )]);
        let events = parse_event_message(&body, SCOPE).expect("parse");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.change.key, "A B/cé.m4b");
        assert_eq!(event.change.kind, ObjectChangeKind::Upserted);
        let info = event.change.info.as_ref().expect("info");
        assert_eq!(info.size, 12);
        assert_eq!(info.etag.as_deref(), Some("\"abc\""));
        assert_eq!(info.signature, info.etag);
        assert_eq!(event.time.as_deref(), Some("2026-11-18T08:00:00.000Z"));
        assert_eq!(event.sequencer.as_deref(), Some("0055"));
    }

    #[test]
    fn deletes_remove_and_minio_names_are_read() {
        let body = records(&[
            record("ObjectRemoved:Delete", "books", "library/a"),
            record("s3:ObjectRemoved:DeleteMarkerCreated", "books", "library/b"),
            record("LifecycleExpiration:Delete", "books", "library/c"),
        ]);
        let events = parse_event_message(&body, SCOPE).expect("parse");
        assert_eq!(events.len(), 3);
        for event in &events {
            assert_eq!(event.change.kind, ObjectChangeKind::Removed);
            assert!(event.change.info.is_none());
        }
    }

    #[test]
    fn deleting_one_version_is_a_change() {
        let body = br#"{"Records":[{"eventName":"ObjectRemoved:Delete","s3":{"bucket":{"name":"books"},"object":{"key":"library/a","versionId":"v1"}}}]}"#;
        let events = parse_event_message(body, SCOPE).expect("parse");
        assert_eq!(events[0].change.kind, ObjectChangeKind::Changed);
    }

    #[test]
    fn other_buckets_keys_outside_the_prefix_and_tag_events_are_skipped() {
        let body = records(&[
            record("ObjectCreated:Put", "other", "library/a"),
            record("ObjectCreated:Put", "books", "elsewhere/a"),
            record("ObjectCreated:Put", "books", "library/"),
            record("ObjectTagging:Put", "books", "library/a"),
            record("ObjectRestore:Completed", "books", "library/b"),
            record("SomethingNew:Happened", "books", "library/c"),
        ]);
        let events = parse_event_message(&body, SCOPE).expect("parse");
        let kinds: Vec<_> = events
            .iter()
            .map(|e| (e.change.key.as_str(), e.change.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("b", ObjectChangeKind::Changed),
                ("c", ObjectChangeKind::Changed)
            ]
        );
    }

    #[test]
    fn the_test_event_is_empty() {
        let body = br#"{"Service":"Amazon S3","Event":"s3:TestEvent","Time":"2026-11-18T08:00:00.000Z","Bucket":"books"}"#;
        assert!(parse_event_message(body, SCOPE).expect("parse").is_empty());
    }

    #[test]
    fn an_sns_envelope_is_unwrapped_once() {
        let inner = String::from_utf8(records(&[record(
            "ObjectCreated:Copy",
            "books",
            "library/a",
        )]))
        .expect("utf8");
        let body = serde_json::json!({"Type": "Notification", "Message": inner}).to_string();
        let events = parse_event_message(body.as_bytes(), SCOPE).expect("parse");
        assert_eq!(events.len(), 1);
        let twice = serde_json::json!({"Type": "Notification", "Message": body}).to_string();
        assert!(matches!(
            parse_event_message(twice.as_bytes(), SCOPE),
            Err(EventParseError::Malformed(_))
        ));
    }

    #[test]
    fn eventbridge_events_are_read_without_decoding() {
        let body = br#"{"version":"0","source":"aws.s3","detail-type":"Object Created","time":"2026-11-18T08:00:00Z","detail":{"bucket":{"name":"books"},"object":{"key":"library/a+b","size":3,"etag":"e","sequencer":"01"}}}"#;
        let events = parse_event_message(body, SCOPE).expect("parse");
        assert_eq!(events[0].change.key, "a+b");
        assert_eq!(events[0].change.kind, ObjectChangeKind::Upserted);
        let deleted = br#"{"source":"aws.s3","detail-type":"Object Deleted","detail":{"bucket":{"name":"books"},"object":{"key":"library/a"}}}"#;
        assert_eq!(
            parse_event_message(deleted, SCOPE).expect("parse")[0]
                .change
                .kind,
            ObjectChangeKind::Removed
        );
        let version = br#"{"source":"aws.s3","detail-type":"Object Deleted","detail":{"deletion-type":"Permanently Deleted","bucket":{"name":"books"},"object":{"key":"library/a","version-id":"v1"}}}"#;
        assert_eq!(
            parse_event_message(version, SCOPE).expect("parse")[0]
                .change
                .kind,
            ObjectChangeKind::Changed
        );
        let marker = br#"{"source":"aws.s3","detail-type":"Object Deleted","detail":{"deletion-type":"Delete Marker Created","bucket":{"name":"books"},"object":{"key":"library/a","version-id":"v1"}}}"#;
        assert_eq!(
            parse_event_message(marker, SCOPE).expect("parse")[0]
                .change
                .kind,
            ObjectChangeKind::Removed
        );
        let tags = br#"{"source":"aws.s3","detail-type":"Object Tags Added","detail":{"bucket":{"name":"books"},"object":{"key":"library/a"}}}"#;
        assert!(parse_event_message(tags, SCOPE).expect("parse").is_empty());
        let other = br#"{"source":"aws.s3","detail-type":"Object Created","detail":{"bucket":{"name":"x"},"object":{"key":"library/a"}}}"#;
        assert!(parse_event_message(other, SCOPE).expect("parse").is_empty());
    }

    #[test]
    fn unreadable_messages_are_errors() {
        for body in [
            &b"not json"[..],
            br#"{"hello":"world"}"#,
            br#"{"Event":"s3:Other"}"#,
            br#"{"Type":"SubscriptionConfirmation","Message":"{}"}"#,
            br#"{"source":"aws.ec2","detail-type":"x","detail":{"bucket":{"name":"books"},"object":{"key":"a"}}}"#,
        ] {
            assert!(
                matches!(parse_event_message(body, SCOPE), Err(EventParseError::Malformed(_))),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let bad_escape = records(&[record("ObjectCreated:Put", "books", "library/%zz")]);
        assert_eq!(
            parse_event_message(&bad_escape, SCOPE),
            Err(EventParseError::BadKey)
        );
        let nul = records(&[record("ObjectCreated:Put", "books", "library/a%00b")]);
        assert_eq!(
            parse_event_message(&nul, SCOPE),
            Err(EventParseError::BadKey)
        );
        let bridge_nul = br#"{"source":"aws.s3","detail-type":"Object Created","detail":{"bucket":{"name":"books"},"object":{"key":"library/a\u0000"}}}"#;
        assert_eq!(
            parse_event_message(bridge_nul, SCOPE),
            Err(EventParseError::BadKey)
        );
        let not_utf8 = records(&[record("ObjectCreated:Put", "books", "library/%ff")]);
        assert_eq!(
            parse_event_message(&not_utf8, SCOPE),
            Err(EventParseError::BadKey)
        );
        let long = records(&[record("ObjectCreated:Put", "books", &"a".repeat(1025))]);
        assert_eq!(
            parse_event_message(&long, SCOPE),
            Err(EventParseError::BadKey)
        );
        let huge = vec![b' '; MAX_EVENT_MESSAGE_BYTES + 1];
        assert_eq!(
            parse_event_message(&huge, SCOPE),
            Err(EventParseError::TooLarge)
        );
        let many: Vec<String> = (0..=MAX_EVENT_RECORDS)
            .map(|i| record("ObjectCreated:Put", "books", &format!("library/{i}")))
            .collect();
        assert_eq!(
            parse_event_message(&records(&many), SCOPE),
            Err(EventParseError::TooManyRecords)
        );
    }

    #[test]
    fn quoted_etags_stay_as_they_are() {
        assert_eq!(quoted("\"x\""), "\"x\"");
        assert_eq!(quoted("x"), "\"x\"");
        assert_eq!(quoted("\""), "\"\"\"");
    }
}
