//! Shared oracle for the `r2_event` fuzz target and the `bookclerk-storage`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is what the `r2` destination reads from Cloudflare: byte 0
//! picks the boundary and the scope, and the rest is the bytes.
//!
//! - Even byte 0: one R2 event notification (a message body). Bit 1 picks
//!   the destination's key prefix (clear: none, set: `library/`).
//! - Odd byte 0: a Queues HTTP pull reply. Bit 1 picks the prefix every
//!   pulled body is then read with as an event.
//!
//! The invariants:
//!
//! - parsing never panics and gives the same answer twice;
//! - an input over its size cap is refused as too large;
//! - every change has a non-empty key without NUL, within the key cap,
//!   with listing fields only on an upsert;
//! - when the event parser accepts a body, an independent reading of the
//!   same JSON (a `serde_json::Value` walk) finds exactly the same change;
//! - when the pull parser accepts a reply, it lists at most the batch cap,
//!   the reply says `success: true`, and each delivery's lease id is the
//!   message's own, non-empty and within its cap, and its body is either
//!   the message text as sent or, decoded with the oracle's own base64
//!   reader, JSON.

use bookclerk_storage::r2_events::{
    parse_pull_reply, parse_r2_event, PullReplyError, R2EventScope, MAX_LEASE_ID_BYTES,
    MAX_PULL_MESSAGES, MAX_PULL_REPLY_BYTES,
};
use bookclerk_storage::s3_events::{EventParseError, MAX_EVENT_KEY_BYTES, MAX_EVENT_MESSAGE_BYTES};
use bookclerk_storage::ObjectChangeKind;
use serde_json::Value;

/// The account every input is checked against.
const ACCOUNT: &str = "3f4b7e3dcab231cbfdaa90a6a28bd548";

/// The bucket every input is checked against.
const BUCKET: &str = "books";

/// One change as the oracle states it: key, kind, size and etag, time.
type Expected = (
    String,
    ObjectChangeKind,
    Option<(u64, Option<String>)>,
    Option<String>,
);

/// Checks one input; panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    let prefix = if selector & 2 == 0 { "" } else { "library/" };
    let scope = R2EventScope {
        account: ACCOUNT,
        bucket: BUCKET,
        prefix,
    };
    if selector % 2 == 0 {
        check_event(rest, scope);
    } else {
        check_reply(rest, scope);
    }
}

/// Checks one event body.
fn check_event(body: &[u8], scope: R2EventScope<'_>) {
    let first = parse_r2_event(body, scope);
    let second = parse_r2_event(body, scope);
    assert_eq!(first, second, "parsing is deterministic");
    if body.len() > MAX_EVENT_MESSAGE_BYTES {
        assert_eq!(first, Err(EventParseError::TooLarge));
        return;
    }
    let Ok(events) = first else {
        return;
    };
    assert!(events.len() <= 1, "one event per message");
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
            assert!(info.version_id.is_none(), "R2 events carry no version");
        }
        assert!(event.sequencer.is_none(), "R2 events carry no sequencer");
    }
    let value: Value = serde_json::from_slice(body).expect("accepted bodies are JSON");
    let expected =
        restate(&value, scope.prefix).expect("the restatement accepts what the parser does");
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
                    .map(|info| (info.size, info.etag.clone())),
                event.time.clone(),
            )
        })
        .collect();
    assert_eq!(got, expected, "parser and restatement disagree");
}

/// The change an R2 event holds, read straight from the JSON tree.
fn restate(value: &Value, prefix: &str) -> Option<Vec<Expected>> {
    let object = value.as_object()?;
    let action = object.get("action")?.as_str()?;
    let bucket = object.get("bucket")?.as_str()?;
    let item = object.get("object")?;
    let key = item.get("key")?.as_str()?;
    let account = object.get("account").and_then(Value::as_str);
    if account.is_some_and(|account| account != ACCOUNT) || bucket != BUCKET {
        return Some(Vec::new());
    }
    let Some(relative) = key.strip_prefix(prefix) else {
        return Some(Vec::new());
    };
    if relative.is_empty() {
        return Some(Vec::new());
    }
    let kind = match action {
        "PutObject" | "CopyObject" | "CompleteMultipartUpload" => ObjectChangeKind::Upserted,
        "DeleteObject" | "LifecycleDeletion" => ObjectChangeKind::Removed,
        _ => ObjectChangeKind::Changed,
    };
    let info = (kind == ObjectChangeKind::Upserted)
        .then(|| item.get("size").and_then(Value::as_u64))
        .flatten()
        .map(|size| {
            let etag = item
                .get("eTag")
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty())
                .map(|e| {
                    if e.len() >= 2 && e.starts_with('"') && e.ends_with('"') {
                        e.to_string()
                    } else {
                        format!("\"{e}\"")
                    }
                });
            (size, etag)
        });
    let time = object
        .get("eventTime")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(vec![(relative.to_string(), kind, info, time)])
}

/// Checks one pull reply, then each body it holds as an event.
fn check_reply(reply: &[u8], scope: R2EventScope<'_>) {
    let first = parse_pull_reply(reply);
    let second = parse_pull_reply(reply);
    assert_eq!(first, second, "parsing is deterministic");
    if reply.len() > MAX_PULL_REPLY_BYTES {
        assert_eq!(first, Err(PullReplyError::TooLarge));
        return;
    }
    if let Err(PullReplyError::Refused(_)) = &first {
        let value: Value = serde_json::from_slice(reply).expect("refusals are JSON");
        assert_eq!(value.get("success"), Some(&Value::Bool(false)));
        return;
    }
    let Ok(messages) = first else {
        return;
    };
    assert!(messages.len() <= MAX_PULL_MESSAGES, "batch over the cap");
    let value: Value = serde_json::from_slice(reply).expect("accepted replies are JSON");
    assert_eq!(value.get("success"), Some(&Value::Bool(true)));
    let listed: Vec<&Value> = value
        .get("result")
        .filter(|r| !r.is_null())
        .and_then(|r| r.get("messages"))
        .filter(|m| !m.is_null())
        .and_then(Value::as_array)
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    assert_eq!(listed.len(), messages.len(), "one delivery per message");
    for (message, wire) in messages.iter().zip(listed) {
        let lease = wire
            .get("lease_id")
            .and_then(Value::as_str)
            .expect("accepted messages have a lease id");
        assert_eq!(message.lease_id, lease);
        assert!(!lease.is_empty() && lease.len() <= MAX_LEASE_ID_BYTES);
        let text = wire
            .get("body")
            .and_then(Value::as_str)
            .expect("accepted messages have a text body");
        if message.body != text.as_bytes() {
            let decoded = base64_decode(text.trim()).expect("a changed body was base64");
            assert_eq!(message.body, decoded, "base64 readers disagree");
            assert!(
                serde_json::from_slice::<serde::de::IgnoredAny>(&decoded).is_ok(),
                "only JSON is decoded"
            );
        }
        check_event(&message.body, scope);
    }
}

/// Standard base64 with `=` padding, written differently from the parser's
/// reader: whole quads only, each sextet looked up by position.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::new();
    let quads = bytes.len() / 4;
    for (index, quad) in bytes.chunks(4).enumerate() {
        let last = index + 1 == quads;
        let pad = quad.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut word = 0u32;
        for &b in &quad[..4 - pad] {
            let sextet = ALPHABET.iter().position(|&a| a == b)?;
            word = (word << 6) | u32::try_from(sextet).ok()?;
        }
        word <<= 6 * u32::try_from(pad).ok()?;
        let [_, a, b, c] = word.to_be_bytes();
        out.extend_from_slice(&[a, b, c][..3 - pad]);
    }
    Some(out)
}
