//! Shared oracle for the `s3_event` fuzz target and the `bookclerk-storage`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one message from an S3 bucket's notification queue. Byte 0
//! picks the destination's key prefix (even: none, odd: `library/`); the
//! rest is the message body. The invariants:
//!
//! - parsing never panics and gives the same answer twice;
//! - a body over the size cap is refused as too large;
//! - every change it reports has a non-empty key without NUL, within the
//!   key cap, with listing fields only on an upsert;
//! - when the parser accepts a message, an independent reading of the same
//!   JSON (a `serde_json::Value` walk with its own percent-decoder) finds
//!   exactly the same changes, in the same order.

use bookclerk_storage::s3_events::{
    parse_event_message, EventParseError, EventScope, MAX_EVENT_KEY_BYTES, MAX_EVENT_MESSAGE_BYTES,
};
use bookclerk_storage::ObjectChangeKind;
use serde_json::Value;

/// The bucket every input is checked against.
const BUCKET: &str = "books";

/// One change as the oracle states it: key, kind, size, etag, version id.
type Expected = (
    String,
    ObjectChangeKind,
    Option<(u64, Option<String>, Option<String>)>,
);

/// Checks one input; panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let prefix = if selector % 2 == 0 { "" } else { "library/" };
    let scope = EventScope {
        bucket: BUCKET,
        prefix,
    };
    let first = parse_event_message(body, scope);
    let second = parse_event_message(body, scope);
    assert_eq!(first, second, "parsing is deterministic");
    if body.len() > MAX_EVENT_MESSAGE_BYTES {
        assert_eq!(first, Err(EventParseError::TooLarge));
        return;
    }
    let Ok(events) = first else {
        return;
    };
    for event in &events {
        let change = &event.change;
        assert!(!change.key.is_empty(), "empty key");
        assert!(!change.key.contains('\0'), "NUL in key");
        assert!(change.key.len() <= MAX_EVENT_KEY_BYTES, "key over the cap");
        if change.kind != ObjectChangeKind::Upserted {
            assert!(change.info.is_none(), "listing fields on a non-upsert");
        }
        if let Some(info) = &change.info {
            assert_eq!(info.key, change.key);
            assert_eq!(info.signature, info.etag);
        }
    }
    let value: Value = serde_json::from_slice(body).expect("accepted bodies are JSON");
    let expected =
        restate(&value, prefix, 0).expect("the restatement accepts what the parser does");
    let got: Vec<Expected> = events
        .iter()
        .map(|event| {
            (
                event.change.key.clone(),
                event.change.kind,
                event
                    .change
                    .info
                    .as_ref()
                    .map(|info| (info.size, info.etag.clone(), info.version_id.clone())),
            )
        })
        .collect();
    assert_eq!(got, expected, "parser and restatement disagree");
}

/// The changes a message holds, read straight from the JSON tree: the
/// shape is the first non-null top-level member of `Records`, `Event`,
/// `Type` and `source`.
fn restate(value: &Value, prefix: &str, depth: u8) -> Option<Vec<Expected>> {
    let object = value.as_object()?;
    let member = |name: &str| object.get(name).filter(|v| !v.is_null());
    if member("Records").is_some() {
        let records = member("Records")?.as_array()?;
        let mut out = Vec::new();
        for record in records {
            let s3 = record.get("s3")?;
            if s3.get("bucket")?.get("name")?.as_str()? != BUCKET {
                continue;
            }
            let item = s3.get("object")?;
            let key = percent_decode(item.get("key")?.as_str()?)?;
            let name = record.get("eventName")?.as_str()?;
            let name = name.strip_prefix("s3:").unwrap_or(name);
            let colon = name.find(':');
            let family = colon.map_or(name, |at| &name[..at]);
            let action = colon.map_or("", |at| &name[at + 1..]);
            let version = text(item, "versionId");
            let kind = match family {
                "ObjectCreated" => Some(ObjectChangeKind::Upserted),
                "ObjectRemoved" if action == "DeleteMarkerCreated" => {
                    Some(ObjectChangeKind::Removed)
                }
                "ObjectRemoved" if version.is_some() => Some(ObjectChangeKind::Changed),
                "ObjectRemoved" | "LifecycleExpiration" => Some(ObjectChangeKind::Removed),
                "ObjectTagging"
                | "ObjectAcl"
                | "LifecycleTransition"
                | "IntelligentTiering"
                | "ObjectAccessed"
                | "s3" => None,
                _ => Some(ObjectChangeKind::Changed),
            };
            push(&mut out, &key, kind, item, "eTag", version, prefix);
        }
        return Some(out);
    }
    if let Some(event) = member("Event") {
        return (event.as_str()? == "s3:TestEvent").then(Vec::new);
    }
    if let Some(kind) = member("Type") {
        if kind.as_str()? != "Notification" || depth > 0 {
            return None;
        }
        let inner: Value = serde_json::from_str(object.get("Message")?.as_str()?).ok()?;
        return restate(&inner, prefix, 1);
    }
    if member("source").and_then(Value::as_str) == Some("aws.s3") {
        let detail = object.get("detail")?;
        if detail.get("bucket")?.get("name")?.as_str()? != BUCKET {
            return Some(Vec::new());
        }
        let item = detail.get("object")?;
        let key = item.get("key")?.as_str()?.to_string();
        let version = text(item, "version-id");
        let permanent =
            detail.get("deletion-type").and_then(Value::as_str) == Some("Permanently Deleted");
        let kind = match object.get("detail-type")?.as_str()? {
            "Object Created" => Some(ObjectChangeKind::Upserted),
            "Object Deleted" if permanent && version.is_some() => Some(ObjectChangeKind::Changed),
            "Object Deleted" => Some(ObjectChangeKind::Removed),
            "Object Tags Added"
            | "Object Tags Deleted"
            | "Object ACL Updated"
            | "Object Storage Class Changed"
            | "Object Access Tier Changed"
            | "Object Restore Initiated"
            | "Object Restore Expired" => None,
            _ => Some(ObjectChangeKind::Changed),
        };
        let mut out = Vec::new();
        push(&mut out, &key, kind, item, "etag", version, prefix);
        return Some(out);
    }
    None
}

/// A non-empty string member, else `None`.
fn text(item: &Value, name: &str) -> Option<String> {
    item.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Appends the change for one event when its key is under `prefix`.
fn push(
    out: &mut Vec<Expected>,
    full_key: &str,
    kind: Option<ObjectChangeKind>,
    item: &Value,
    etag_field: &str,
    version: Option<String>,
    prefix: &str,
) {
    let Some(kind) = kind else { return };
    let Some(key) = full_key.strip_prefix(prefix).filter(|k| !k.is_empty()) else {
        return;
    };
    let info = (kind == ObjectChangeKind::Upserted)
        .then(|| item.get("size").and_then(Value::as_u64))
        .flatten()
        .map(|size| {
            let etag = text(item, etag_field).map(|e| {
                if e.len() >= 2 && e.starts_with('"') && e.ends_with('"') {
                    e
                } else {
                    format!("\"{e}\"")
                }
            });
            (size, etag, version)
        });
    out.push((key.to_string(), kind, info));
}

/// Form decoding written differently from the parser's: `+` is a space and
/// `%XX` a byte, read with `u8::from_str_radix`.
fn percent_decode(encoded: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut rest = encoded;
    while let Some(c) = rest.chars().next() {
        if c == '%' {
            let hex = rest.get(1..3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &rest[3..];
        } else {
            let mut buf = [0u8; 4];
            let s = if c == '+' {
                " "
            } else {
                c.encode_utf8(&mut buf)
            };
            bytes.extend_from_slice(s.as_bytes());
            rest = &rest[c.len_utf8()..];
        }
    }
    String::from_utf8(bytes).ok()
}
