//! Cloudflare R2 event notifications and Queues pull replies, parsed into
//! change-feed entries.
//!
//! An R2 bucket announces object writes and deletes on a Cloudflare Queue
//! (an event notification rule), and the `r2` destination drains that queue
//! through its HTTP pull consumer to serve `changes`. Both the pull reply
//! and each message in it come from outside Bookclerk, so this module is
//! the boundary that reads them: pure functions over bytes, with no I/O,
//! fuzzed by the `r2_event` target.
//!
//! A pull reply (`{"success": true, "result": {"messages": [...]}}`) lists
//! messages with a body and the lease id that acknowledges or retries the
//! delivery. A body published as `json` or `bytes` comes base64-encoded and
//! one published as `text` comes as it is; this module accepts either, and
//! reads the decoded text as an R2 event:
//!
//! ```json
//! {"account": "...", "action": "PutObject", "bucket": "books",
//!  "object": {"key": "library/a.m4b", "size": 12, "eTag": "abc"},
//!  "eventTime": "2026-11-18T08:00:00.000Z"}
//! ```
//!
//! Events for another account or bucket, or for keys outside the
//! destination prefix, are skipped. The feed still acknowledges the message
//! that held them, so a queue belongs to one destination: a second reader
//! would lose whatever the first one skipped.
//! Anything else unreadable is an error: the caller counts the message as a
//! change it could not read.

use base64::Engine;
use serde::Deserialize;

use crate::s3_events::{
    event_for, Effect, EventParseError, ObjectFields, ParsedEvent, MAX_EVENT_KEY_BYTES,
    MAX_EVENT_MESSAGE_BYTES,
};

/// Largest pull reply read, in bytes: a full batch of the largest messages
/// Queues accepts (128 KiB each, base64-encoded) with room for the
/// envelope.
pub const MAX_PULL_REPLY_BYTES: usize = 24 * 1024 * 1024;

/// Most messages one pull reply may list (the Queues batch maximum).
pub const MAX_PULL_MESSAGES: usize = 100;

/// Longest lease id accepted, in bytes.
pub const MAX_LEASE_ID_BYTES: usize = 4096;

/// Which objects an `r2` destination owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct R2EventScope<'a> {
    /// Cloudflare account id; events naming another account are skipped.
    pub account: &'a str,
    /// Bucket name; events for other buckets are skipped.
    pub bucket: &'a str,
    /// Normalized key prefix (empty, or ending in `/`); it is stripped from
    /// every key, and keys outside it are skipped.
    pub prefix: &'a str,
}

/// One delivery listed in a pull reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledMessage {
    /// The message body, decoded from base64 when it was encoded.
    pub body: Vec<u8>,
    /// The lease that acknowledges or retries this delivery.
    pub lease_id: String,
}

/// Why a pull reply could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PullReplyError {
    /// The reply is over [`MAX_PULL_REPLY_BYTES`].
    #[error("queue pull reply is over {MAX_PULL_REPLY_BYTES} bytes")]
    TooLarge,
    /// The reply is not a pull reply.
    #[error("queue pull reply is malformed: {0}")]
    Malformed(String),
    /// The API reported failure.
    #[error("queue pull failed: {0}")]
    Refused(String),
    /// The reply lists more than [`MAX_PULL_MESSAGES`] messages.
    #[error("queue pull reply lists more than {MAX_PULL_MESSAGES} messages")]
    TooManyMessages,
}

/// A Cloudflare API reply envelope.
#[derive(Deserialize)]
struct Envelope {
    /// Whether the call succeeded.
    success: bool,
    /// Errors the API reported.
    #[serde(default)]
    errors: Vec<ApiError>,
    /// The pull result.
    #[serde(default)]
    result: Option<PullResult>,
}

/// One error in an API reply.
#[derive(Deserialize)]
struct ApiError {
    /// Error code.
    #[serde(default)]
    code: Option<i64>,
    /// Error message.
    #[serde(default)]
    message: Option<String>,
}

/// The `result` of a pull reply.
#[derive(Deserialize)]
struct PullResult {
    /// The pulled messages.
    #[serde(default)]
    messages: Vec<WireMessage>,
}

/// One message as a pull reply lists it.
#[derive(Deserialize)]
struct WireMessage {
    /// The body: base64 for `json` and `bytes` messages, plain for `text`.
    body: String,
    /// The lease id.
    lease_id: String,
}

/// Reads a Queues pull reply.
///
/// The reply is read as a JSON tree first, so a repeated member counts
/// once, as its last value, and the reply and each message must be JSON
/// objects (not the arrays serde would otherwise take for a struct).
///
/// # Arguments
///
/// * `reply` - the HTTP response body.
///
/// # Returns
///
/// The deliveries in the order the reply lists them.
///
/// # Errors
///
/// Returns a [`PullReplyError`] when the reply is too large, malformed,
/// reports failure, or lists too many messages. A message whose lease id
/// is empty or over [`MAX_LEASE_ID_BYTES`] makes the whole reply malformed,
/// since it could be neither acknowledged nor retried.
pub fn parse_pull_reply(reply: &[u8]) -> Result<Vec<PulledMessage>, PullReplyError> {
    if reply.len() > MAX_PULL_REPLY_BYTES {
        return Err(PullReplyError::TooLarge);
    }
    let malformed = |err: serde_json::Error| PullReplyError::Malformed(err.to_string());
    let value: serde_json::Value = serde_json::from_slice(reply).map_err(malformed)?;
    let result = value.get("result").filter(|result| !result.is_null());
    let messages = result
        .and_then(|result| result.get("messages"))
        .and_then(serde_json::Value::as_array);
    if !value.is_object()
        || result.is_some_and(|result| !result.is_object())
        || messages.is_some_and(|messages| messages.iter().any(|m| !m.is_object()))
    {
        return Err(PullReplyError::Malformed(
            "a pull reply and its messages are JSON objects".into(),
        ));
    }
    let envelope = Envelope::deserialize(&value).map_err(malformed)?;
    if !envelope.success {
        let detail = envelope
            .errors
            .iter()
            .map(|err| {
                format!(
                    "{} {}",
                    err.code.map(|code| code.to_string()).unwrap_or_default(),
                    err.message.as_deref().unwrap_or_default()
                )
                .trim()
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(PullReplyError::Refused(detail));
    }
    let messages = envelope.result.map(|r| r.messages).unwrap_or_default();
    if messages.len() > MAX_PULL_MESSAGES {
        return Err(PullReplyError::TooManyMessages);
    }
    messages
        .into_iter()
        .map(|message| {
            if message.lease_id.is_empty() || message.lease_id.len() > MAX_LEASE_ID_BYTES {
                return Err(PullReplyError::Malformed("bad lease id".into()));
            }
            Ok(PulledMessage {
                body: decode_body(&message.body),
                lease_id: message.lease_id,
            })
        })
        .collect()
}

/// A message body as published: base64 decoded when it is base64 of a
/// JSON value, else the text itself (a `text` message, or a reply that
/// sends `json` messages unencoded).
fn decode_body(body: &str) -> Vec<u8> {
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(body.trim()) {
        if serde_json::from_slice::<serde::de::IgnoredAny>(&decoded).is_ok() {
            return decoded;
        }
    }
    body.as_bytes().to_vec()
}

/// An R2 event notification.
#[derive(Deserialize)]
struct R2Event {
    /// The account the event belongs to.
    #[serde(default)]
    account: Option<String>,
    /// What happened (`PutObject`, `DeleteObject`, ...).
    action: String,
    /// The bucket.
    bucket: String,
    /// The object.
    object: R2Object,
    /// When it happened (ISO 8601).
    #[serde(rename = "eventTime", default)]
    event_time: Option<String>,
}

/// The object an R2 event names.
#[derive(Deserialize)]
struct R2Object {
    /// The key, as stored (not encoded).
    key: String,
    /// Size in bytes; absent for deletes.
    #[serde(default)]
    size: Option<u64>,
    /// Entity tag without quotes; absent for deletes.
    #[serde(rename = "eTag", default)]
    etag: Option<String>,
}

/// Reads one R2 event notification into changes for `scope`.
///
/// The body is read as a JSON tree first, so a repeated member counts once,
/// as its last value, and the event and its `object` must be JSON objects.
///
/// # Arguments
///
/// * `body` - the decoded message body.
/// * `scope` - the destination's account, bucket and key prefix.
///
/// # Returns
///
/// The change for an object under `scope` (R2 sends one event per
/// message), or nothing for another account, bucket or key.
///
/// # Errors
///
/// Returns an [`EventParseError`] when the body is too large, is not an R2
/// event, or names an unreadable key.
pub fn parse_r2_event(
    body: &[u8],
    scope: R2EventScope<'_>,
) -> Result<Vec<ParsedEvent>, EventParseError> {
    if body.len() > MAX_EVENT_MESSAGE_BYTES {
        return Err(EventParseError::TooLarge);
    }
    let malformed = |err: serde_json::Error| EventParseError::Malformed(err.to_string());
    let value: serde_json::Value = serde_json::from_slice(body).map_err(malformed)?;
    if !value.is_object()
        || !value
            .get("object")
            .is_some_and(serde_json::Value::is_object)
    {
        return Err(EventParseError::Malformed(
            "an R2 event and its object are JSON objects".into(),
        ));
    }
    let event = R2Event::deserialize(&value).map_err(malformed)?;
    if event
        .account
        .as_deref()
        .is_some_and(|account| account != scope.account)
        || event.bucket != scope.bucket
    {
        return Ok(Vec::new());
    }
    // Scope comes before validation: a broken key outside this destination
    // must not fail the message.
    if !event.object.key.starts_with(scope.prefix) {
        return Ok(Vec::new());
    }
    if event.object.key.len() > MAX_EVENT_KEY_BYTES || event.object.key.contains('\0') {
        return Err(EventParseError::BadKey);
    }
    let effect = match event.action.as_str() {
        "PutObject" | "CopyObject" | "CompleteMultipartUpload" => Effect::Upserted,
        "DeleteObject" | "LifecycleDeletion" => Effect::Removed,
        // An action R2 adds later: the host reads the object to settle it.
        _ => Effect::Changed,
    };
    Ok(event_for(
        event.object.key,
        effect,
        crate::s3_events::EventScope {
            bucket: scope.bucket,
            prefix: scope.prefix,
        },
        ObjectFields {
            size: event.object.size,
            etag: event.object.etag,
            version_id: None,
        },
        event.event_time,
        None,
    )?
    .into_iter()
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectChangeKind;

    const SCOPE: R2EventScope<'static> = R2EventScope {
        account: "3f4b7e3dcab231cbfdaa90a6a28bd548",
        bucket: "books",
        prefix: "library/",
    };

    fn event(action: &str, key: &str) -> String {
        format!(
            r#"{{"account":"3f4b7e3dcab231cbfdaa90a6a28bd548","action":"{action}","bucket":"books","object":{{"key":"{key}","size":65536,"eTag":"c846ff7a"}},"eventTime":"2024-05-24T19:36:44.379Z"}}"#
        )
    }

    #[test]
    fn writes_are_upserts_with_listing_fields() {
        for action in ["PutObject", "CopyObject", "CompleteMultipartUpload"] {
            let parsed =
                parse_r2_event(event(action, "library/a b.m4b").as_bytes(), SCOPE).unwrap();
            assert_eq!(parsed.len(), 1, "{action}");
            let change = &parsed[0].change;
            assert_eq!(change.key, "a b.m4b", "keys are not form-encoded");
            assert_eq!(change.kind, ObjectChangeKind::Upserted);
            let info = change.info.as_ref().unwrap();
            assert_eq!(info.size, 65536);
            assert_eq!(info.etag.as_deref(), Some("\"c846ff7a\""));
            assert_eq!(parsed[0].time.as_deref(), Some("2024-05-24T19:36:44.379Z"));
        }
    }

    #[test]
    fn deletes_remove_and_unknown_actions_change() {
        let body = r#"{"account":"3f4b7e3dcab231cbfdaa90a6a28bd548","action":"DeleteObject","bucket":"books","object":{"key":"library/a"},"eventTime":"2024-05-24T19:36:44.379Z"}"#;
        let parsed = parse_r2_event(body.as_bytes(), SCOPE).unwrap();
        assert_eq!(parsed[0].change.kind, ObjectChangeKind::Removed);
        assert!(parsed[0].change.info.is_none());
        let lifecycle = event("LifecycleDeletion", "library/a");
        assert_eq!(
            parse_r2_event(lifecycle.as_bytes(), SCOPE).unwrap()[0]
                .change
                .kind,
            ObjectChangeKind::Removed
        );
        let later = event("RestoreObject", "library/a");
        let parsed = parse_r2_event(later.as_bytes(), SCOPE).unwrap();
        assert_eq!(parsed[0].change.kind, ObjectChangeKind::Changed);
        assert!(parsed[0].change.info.is_none());
    }

    #[test]
    fn other_accounts_buckets_and_keys_are_skipped() {
        let other_account = event("PutObject", "library/a").replace("3f4b7e3d", "00000000");
        assert!(parse_r2_event(other_account.as_bytes(), SCOPE)
            .unwrap()
            .is_empty());
        let other_bucket = event("PutObject", "library/a").replace("\"books\"", "\"music\"");
        assert!(parse_r2_event(other_bucket.as_bytes(), SCOPE)
            .unwrap()
            .is_empty());
        let outside = event("PutObject", "elsewhere/a");
        assert!(parse_r2_event(outside.as_bytes(), SCOPE)
            .unwrap()
            .is_empty());
        // The prefix itself names no object.
        assert!(
            parse_r2_event(event("PutObject", "library/").as_bytes(), SCOPE)
                .unwrap()
                .is_empty()
        );
        // A broken key outside the scope does not fail the message.
        let long = format!("elsewhere/{}", "a".repeat(MAX_EVENT_KEY_BYTES));
        assert!(parse_r2_event(event("PutObject", &long).as_bytes(), SCOPE)
            .unwrap()
            .is_empty());
        // An event without an account is read for the bucket alone.
        let no_account = event("PutObject", "library/a")
            .replace("\"account\":\"3f4b7e3dcab231cbfdaa90a6a28bd548\",", "");
        assert_eq!(
            parse_r2_event(no_account.as_bytes(), SCOPE).unwrap().len(),
            1
        );
    }

    #[test]
    fn unreadable_events_are_errors() {
        let long = format!("library/{}", "a".repeat(MAX_EVENT_KEY_BYTES));
        assert_eq!(
            parse_r2_event(event("PutObject", &long).as_bytes(), SCOPE),
            Err(EventParseError::BadKey)
        );
        assert_eq!(
            parse_r2_event(event("PutObject", "library/a\\u0000b").as_bytes(), SCOPE),
            Err(EventParseError::BadKey)
        );
        assert!(matches!(
            parse_r2_event(b"{}", SCOPE),
            Err(EventParseError::Malformed(_))
        ));
        assert!(matches!(
            parse_r2_event(b"[1]", SCOPE),
            Err(EventParseError::Malformed(_))
        ));
        // serde would read a struct from an array; an event is an object.
        let as_array = br#"["PutObject","books",["library/a",1,"e"]]"#;
        assert!(matches!(
            parse_r2_event(as_array, SCOPE),
            Err(EventParseError::Malformed(_))
        ));
        let object_as_array = br#"{"action":"PutObject","bucket":"books","object":["library/a"]}"#;
        assert!(matches!(
            parse_r2_event(object_as_array, SCOPE),
            Err(EventParseError::Malformed(_))
        ));
        let huge = vec![b' '; MAX_EVENT_MESSAGE_BYTES + 1];
        assert_eq!(parse_r2_event(&huge, SCOPE), Err(EventParseError::TooLarge));
    }

    #[test]
    fn pull_replies_decode_base64_and_text_bodies() {
        let event = event("PutObject", "library/a");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&event);
        let reply = format!(
            r#"{{"success":true,"errors":[],"messages":[],"result":{{"message_backlog_count":2,"messages":[
                {{"body":"{encoded}","id":"m1","timestamp_ms":1,"attempts":1,"lease_id":"l1","metadata":{{"CF-Content-Type":"json"}}}},
                {{"body":"hello","id":"m2","timestamp_ms":2,"attempts":2,"lease_id":"l2","metadata":{{"CF-Content-Type":"text"}}}}
            ]}}}}"#
        );
        let messages = parse_pull_reply(reply.as_bytes()).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].body, event.as_bytes());
        assert_eq!(messages[0].lease_id, "l1");
        assert_eq!(messages[1].body, b"hello", "text stays as it is");
        assert_eq!(
            parse_r2_event(&messages[0].body, SCOPE).unwrap()[0]
                .change
                .key,
            "a"
        );
        // An unencoded JSON body is read as it is.
        let plain = serde_json::json!({
            "success": true,
            "result": {"messages": [{"body": event, "lease_id": "l3"}]},
        });
        let messages = parse_pull_reply(plain.to_string().as_bytes()).unwrap();
        assert_eq!(messages[0].body, event.as_bytes());
    }

    #[test]
    fn empty_and_refused_and_malformed_replies() {
        assert!(
            parse_pull_reply(br#"{"success":true,"result":{"messages":[]}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_pull_reply(br#"{"success":true}"#).unwrap().is_empty());
        let refused = parse_pull_reply(
            br#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}]}"#,
        );
        assert_eq!(
            refused,
            Err(PullReplyError::Refused("10000 Authentication error".into()))
        );
        for bad in [
            &br#"{"result":{}}"#[..],
            br#"not json"#,
            br#"{"success":true,"result":{"messages":[{"body":"x"}]}}"#,
            br#"{"success":true,"result":{"messages":[{"body":"x","lease_id":""}]}}"#,
            br#"[true,[],{"messages":[]}]"#,
            br#"{"success":true,"result":[[]]}"#,
            br#"{"success":true,"result":{"messages":[["x","l1"]]}}"#,
        ] {
            assert!(
                matches!(parse_pull_reply(bad), Err(PullReplyError::Malformed(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let many: Vec<_> = (0..=MAX_PULL_MESSAGES)
            .map(|i| serde_json::json!({"body": "x", "lease_id": format!("l{i}")}))
            .collect();
        let reply = serde_json::json!({"success": true, "result": {"messages": many}});
        assert_eq!(
            parse_pull_reply(reply.to_string().as_bytes()),
            Err(PullReplyError::TooManyMessages)
        );
        let huge = vec![b' '; MAX_PULL_REPLY_BYTES + 1];
        assert_eq!(parse_pull_reply(&huge), Err(PullReplyError::TooLarge));
    }
}
