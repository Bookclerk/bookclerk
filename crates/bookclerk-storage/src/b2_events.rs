//! Backblaze B2 event notifications and notification-rule replies, read
//! into checked types.
//!
//! A B2 bucket announces object writes, deletes and hide markers by posting
//! signed batches of events to a webhook (an event notification rule). The
//! host checks the signature and stores each request, and the `b2`
//! destination reads the stored bodies to serve `changes`. The guest also
//! reads the bucket's rules back from `b2_get_bucket_notification_rules` and
//! `b2_set_bucket_notification_rules` to keep its own rules in place. Both
//! come from outside Bookclerk, so this module is the boundary that reads
//! them: pure functions over bytes, with no I/O, fuzzed by the `b2_event`
//! target.
//!
//! A webhook body lists events:
//!
//! ```json
//! {"events": [{"eventType": "b2:ObjectCreated:Upload", "bucketName": "books",
//!   "objectName": "library/a.m4b", "eventTimestamp": 1684793309123,
//!   "eventId": "...", "objectSize": 12, "objectVersionId": "4_z..."}]}
//! ```
//!
//! Every event the feed reports becomes a `changed` entry with no object
//! details: B2 may deliver events late, twice or out of order, and an event
//! for one version says little about the name's current version (deleting
//! a version can uncover an older one), so the host probes each key.
//! Events of other types, for another bucket, or for names outside the
//! destination prefix are skipped. Anything else unreadable is an error:
//! the caller counts the delivery as a change it could not read.

use std::fmt;

use serde_json::{json, Map, Value};
use sha2::Digest;

use crate::b2_api::{check_file_name, B2ReplyError, ReplyResult, MAX_B2_REPLY_BYTES};
use crate::s3_events::{EventParseError, ParsedEvent, MAX_EVENT_MESSAGE_BYTES, MAX_EVENT_RECORDS};
use crate::{ObjectChange, ObjectChangeKind};

/// Most events read from one webhook body. B2 batches at most 50; the cap
/// bounds what a crafted body can make the feed hold.
pub const MAX_B2_EVENTS: usize = MAX_EVENT_RECORDS;

/// Most rules one rules reply may list. B2 allows 25 per bucket.
pub const MAX_NOTIFICATION_RULES: usize = 100;

/// Length of a rule's signing secret, in characters.
pub const SIGNING_SECRET_LEN: usize = 32;

/// Events per webhook request the destination's rules ask for (B2's
/// maximum).
pub const EVENTS_PER_BATCH: u32 = 50;

/// The event-type categories the feed reports, each covered by one of the
/// destination's rules (B2 refuses a rule that mixes categories).
pub const EVENT_CATEGORIES: [(&str, &str); 3] = [
    ("b2:ObjectCreated", "created"),
    ("b2:ObjectDeleted", "deleted"),
    ("b2:HideMarkerCreated", "hidden"),
];

/// Which objects a `b2` destination owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B2EventScope<'a> {
    /// Bucket name; events for other buckets are skipped.
    pub bucket: &'a str,
    /// Normalized key prefix (empty, or ending in `/`); it is stripped from
    /// every name, and names outside it are skipped.
    pub prefix: &'a str,
}

/// Whether the feed reports events of `event_type`: one of
/// [`EVENT_CATEGORIES`] with a non-empty third part. Test events, live-read
/// uploads and types B2 adds in new categories are not reported.
fn reported(event_type: &str) -> bool {
    let mut parts = event_type.splitn(3, ':');
    let (Some(vendor), Some(category), Some(detail)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    vendor == "b2"
        && !detail.is_empty()
        && EVENT_CATEGORIES
            .iter()
            .any(|(known, _)| known.strip_prefix("b2:") == Some(category))
}

/// Reads one webhook body into changes for `scope`, in the order it lists
/// them.
///
/// The body is read as a JSON tree first, so a repeated member counts once,
/// as its last value, and the body and each event must be JSON objects.
///
/// # Arguments
///
/// * `body` - the request body as the host stored it.
/// * `scope` - the destination's bucket and key prefix.
///
/// # Returns
///
/// A `changed` entry per reported event under `scope`, carrying the event
/// time as twenty zero-padded digits of Unix milliseconds (so times compare
/// as text), when the event states one.
///
/// # Errors
///
/// Returns an [`EventParseError`] when the body is too large, is not a list
/// of events, lists too many, or names an unreadable object.
pub fn parse_b2_events(
    body: &[u8],
    scope: B2EventScope<'_>,
) -> Result<Vec<ParsedEvent>, EventParseError> {
    if body.len() > MAX_EVENT_MESSAGE_BYTES {
        return Err(EventParseError::TooLarge);
    }
    let malformed = |what: &str| EventParseError::Malformed(format!("B2 events: {what}"));
    let value: Value =
        serde_json::from_slice(body).map_err(|err| EventParseError::Malformed(err.to_string()))?;
    let events = value
        .as_object()
        .and_then(|top| top.get("events"))
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("the body is an object with an events list"))?;
    if events.len() > MAX_B2_EVENTS {
        return Err(EventParseError::TooManyRecords);
    }
    let mut out = Vec::new();
    for event in events {
        let event = event
            .as_object()
            .ok_or_else(|| malformed("each event is an object"))?;
        let text = |field: &str| {
            event
                .get(field)
                .and_then(Value::as_str)
                .ok_or_else(|| malformed(&format!("an event needs its {field}")))
        };
        if !reported(text("eventType")?) || text("bucketName")? != scope.bucket {
            continue;
        }
        let name = text("objectName")?;
        // Scope comes before validation: a broken name outside this
        // destination must not fail the delivery.
        let Some(key) = name
            .strip_prefix(scope.prefix)
            .filter(|key| !key.is_empty())
        else {
            continue;
        };
        check_file_name(name).map_err(|_| EventParseError::BadKey)?;
        let time = match event.get("eventTimestamp") {
            None | Some(Value::Null) => None,
            Some(stamp) => Some(format!(
                "{:020}",
                stamp
                    .as_u64()
                    .ok_or_else(|| malformed("an event time is a whole number"))?
            )),
        };
        out.push(ParsedEvent {
            change: ObjectChange {
                key: key.to_string(),
                kind: ObjectChangeKind::Changed,
                info: None,
            },
            time,
            sequencer: None,
        });
    }
    Ok(out)
}

/// The shared secret B2 signs a rule's webhook requests with: 32 ASCII
/// letters or digits, as B2 requires. Its `Debug` output never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct B2SigningSecret(String);

impl B2SigningSecret {
    /// Checks a secret.
    ///
    /// # Errors
    ///
    /// Returns a message when the secret is not [`SIGNING_SECRET_LEN`]
    /// ASCII letters or digits.
    pub fn parse(secret: &str) -> Result<Self, String> {
        if secret.len() == SIGNING_SECRET_LEN && secret.bytes().all(|b| b.is_ascii_alphanumeric()) {
            Ok(Self(secret.to_string()))
        } else {
            Err(format!(
                "the webhook signing secret must be {SIGNING_SECRET_LEN} letters or digits"
            ))
        }
    }

    /// The secret itself, for the rule B2 stores.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for B2SigningSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("B2SigningSecret(<redacted>)")
    }
}

/// One event notification rule as B2 lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationRule {
    /// The rule's name, unique in the bucket.
    pub name: String,
    /// The event types it matches.
    pub event_types: Vec<String>,
    /// The name prefix it matches; empty for every name.
    pub object_name_prefix: String,
    /// Whether it is turned on.
    pub is_enabled: bool,
    /// Whether B2 suspended it after failed deliveries; a suspended rule
    /// sends nothing and can only be deleted.
    pub is_suspended: bool,
    /// The webhook URL, for a webhook target.
    pub url: Option<String>,
    /// The signing secret, when the rule has one and it is well formed.
    pub signing_secret: Option<B2SigningSecret>,
    /// Events per request.
    pub max_events_per_batch: Option<u64>,
    /// The rule as listed, without the fields B2 computes, so a rule that is
    /// not the destination's own can be sent back unchanged (setting rules
    /// replaces them all).
    pub raw: Map<String, Value>,
}

/// Reads a `b2_get_bucket_notification_rules` or
/// `b2_set_bucket_notification_rules` reply.
///
/// # Arguments
///
/// * `bytes` - the reply body.
///
/// # Returns
///
/// The bucket's rules, in the order the reply lists them.
///
/// # Errors
///
/// Returns [`B2ReplyError`] for an oversized reply, one that is not a JSON
/// object with a list of rule objects, more than
/// [`MAX_NOTIFICATION_RULES`] rules, a rule without a name, or two rules
/// with one name.
pub fn parse_notification_rules(bytes: &[u8]) -> ReplyResult<Vec<NotificationRule>> {
    if bytes.len() > MAX_B2_REPLY_BYTES {
        return Err(B2ReplyError::TooLarge);
    }
    let malformed = |what: &str| B2ReplyError::Malformed(format!("notification rules: {what}"));
    let text = std::str::from_utf8(bytes).map_err(|_| malformed("not UTF-8"))?;
    let value: Value = serde_json::from_str(text).map_err(|err| malformed(&err.to_string()))?;
    let rules = value
        .as_object()
        .and_then(|top| top.get("eventNotificationRules"))
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("the reply is an object with a list of rules"))?;
    if rules.len() > MAX_NOTIFICATION_RULES {
        return Err(malformed("too many rules"));
    }
    let mut out: Vec<NotificationRule> = Vec::with_capacity(rules.len());
    for rule in rules {
        let rule = rule
            .as_object()
            .ok_or_else(|| malformed("each rule is an object"))?;
        let name = rule
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && name.len() <= 63)
            .ok_or_else(|| malformed("each rule has a name of 1 to 63 bytes"))?
            .to_string();
        if out.iter().any(|seen| seen.name == name) {
            return Err(malformed("two rules share a name"));
        }
        let target = rule.get("targetConfiguration").and_then(Value::as_object);
        let target_text = |field: &str| {
            target
                .and_then(|target| target.get(field))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let mut raw = rule.clone();
        raw.remove("isSuspended");
        raw.remove("suspensionReason");
        out.push(NotificationRule {
            name,
            event_types: rule
                .get("eventTypes")
                .and_then(Value::as_array)
                .map(|types| {
                    types
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            object_name_prefix: rule
                .get("objectNamePrefix")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            is_enabled: rule
                .get("isEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_suspended: rule
                .get("isSuspended")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            url: target_text("url"),
            signing_secret: target_text("hmacSha256SigningSecret")
                .and_then(|secret| B2SigningSecret::parse(&secret).ok()),
            max_events_per_batch: target
                .and_then(|target| target.get("maxEventsPerBatch"))
                .and_then(Value::as_u64),
            raw,
        });
    }
    Ok(out)
}

/// The names of the destination's rules for `prefix`: `bookclerk-`, 16 hex
/// digits of the prefix's SHA-256, and the category, one per entry of
/// [`EVENT_CATEGORIES`]. B2 refuses two rules for one prefix and event type,
/// so the names follow the prefix, not the instance.
#[must_use]
pub fn rule_names(prefix: &str) -> [String; 3] {
    let digest = sha2::Sha256::digest(prefix.as_bytes());
    let tag = hex::encode(&digest[..8]);
    EVENT_CATEGORIES.map(|(_, suffix)| format!("bookclerk-{tag}-{suffix}"))
}

/// The destination's rules for `prefix`, posting to `url` signed with
/// `secret`.
#[must_use]
pub fn own_rules(prefix: &str, url: &str, secret: &B2SigningSecret) -> Vec<Value> {
    rule_names(prefix)
        .into_iter()
        .zip(EVENT_CATEGORIES)
        .map(|(name, (category, _))| {
            json!({
                "name": name,
                "eventTypes": [format!("{category}:*")],
                "isEnabled": true,
                "objectNamePrefix": prefix,
                "targetConfiguration": {
                    "targetType": "webhook",
                    "url": url,
                    "hmacSha256SigningSecret": secret.expose(),
                    "customHeaders": [],
                    "maxEventsPerBatch": EVENTS_PER_BATCH,
                },
            })
        })
        .collect()
}

/// True when `existing` holds the destination's rules for `prefix` as
/// [`own_rules`] would write them, turned on and not suspended.
///
/// B2 does not document whether a listed rule repeats its signing secret
/// and batch size, so a field the listing leaves out is not held against
/// the rule; one it gives must match.
#[must_use]
pub fn own_rules_in_place(
    existing: &[NotificationRule],
    prefix: &str,
    url: &str,
    secret: &B2SigningSecret,
) -> bool {
    rule_names(prefix)
        .iter()
        .zip(EVENT_CATEGORIES)
        .all(|(name, (category, _))| {
            existing.iter().any(|rule| {
                rule.name == *name
                    && rule.is_enabled
                    && !rule.is_suspended
                    && rule.event_types == [format!("{category}:*")]
                    && rule.object_name_prefix == prefix
                    && rule.url.as_deref() == Some(url)
                    && rule
                        .raw
                        .get("targetConfiguration")
                        .and_then(|target| target.get("hmacSha256SigningSecret"))
                        .is_none_or(|got| got.as_str() == Some(secret.expose()))
                    && rule
                        .max_events_per_batch
                        .is_none_or(|got| got == u64::from(EVENTS_PER_BATCH))
            })
        })
}

/// The rules to send back to keep every rule in `existing` except the
/// destination's own for `prefix`, then add `own` (empty to remove them).
/// A suspended rule of the destination's is dropped too: B2 lets it only
/// be deleted.
#[must_use]
pub fn rules_with(existing: &[NotificationRule], prefix: &str, own: Vec<Value>) -> Vec<Value> {
    let names = rule_names(prefix);
    existing
        .iter()
        .filter(|rule| !names.contains(&rule.name))
        .map(|rule| Value::Object(rule.raw.clone()))
        .chain(own)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: B2EventScope<'static> = B2EventScope {
        bucket: "books",
        prefix: "library/",
    };

    fn keys(body: &str) -> Vec<(String, Option<String>)> {
        parse_b2_events(body.as_bytes(), SCOPE)
            .expect("parse")
            .into_iter()
            .map(|event| {
                assert_eq!(event.change.kind, ObjectChangeKind::Changed);
                assert!(event.change.info.is_none());
                (event.change.key, event.time)
            })
            .collect()
    }

    #[test]
    fn reported_events_under_the_prefix_become_changes() {
        let body = r#"{"events":[
            {"eventType":"b2:ObjectCreated:Upload","bucketName":"books","objectName":"library/a b.m4b","eventTimestamp":1684793309123},
            {"eventType":"b2:ObjectDeleted:Delete","bucketName":"books","objectName":"library/c"},
            {"eventType":"b2:HideMarkerCreated:Hide","bucketName":"books","objectName":"library/d","eventTimestamp":null},
            {"eventType":"b2:ObjectCreated:SomethingNew","bucketName":"books","objectName":"library/e"},
            {"eventType":"b2:MultipartUploadCreated:LiveRead","bucketName":"books","objectName":"library/f"},
            {"eventType":"b2:TestEvent","bucketName":"books"},
            {"eventType":"b2:ObjectCreated:Upload","bucketName":"other","objectName":"library/g"},
            {"eventType":"b2:ObjectCreated:Upload","bucketName":"books","objectName":"elsewhere/\u0000"},
            {"eventType":"b2:ObjectCreated:Upload","bucketName":"books","objectName":"library/"}
        ]}"#;
        assert_eq!(
            keys(body),
            vec![
                ("a b.m4b".into(), Some("00000001684793309123".into())),
                ("c".into(), None),
                ("d".into(), None),
                ("e".into(), None),
            ]
        );
    }

    #[test]
    fn unreadable_bodies_are_errors() {
        for body in [
            "[]",
            "{}",
            r#"{"events":{}}"#,
            r#"{"events":[1]}"#,
            r#"{"events":[{"bucketName":"books","objectName":"library/a"}]}"#,
            r#"{"events":[{"eventType":"b2:ObjectCreated:Upload","objectName":"library/a"}]}"#,
            r#"{"events":[{"eventType":"b2:ObjectCreated:Upload","bucketName":"books"}]}"#,
            r#"{"events":[{"eventType":"b2:ObjectCreated:Upload","bucketName":"books","objectName":"library/a","eventTimestamp":-1}]}"#,
        ] {
            assert!(
                matches!(
                    parse_b2_events(body.as_bytes(), SCOPE),
                    Err(EventParseError::Malformed(_))
                ),
                "{body}"
            );
        }
        let bad_name = r#"{"events":[{"eventType":"b2:ObjectCreated:Upload","bucketName":"books","objectName":"library/a\\b"}]}"#;
        assert_eq!(
            parse_b2_events(bad_name.as_bytes(), SCOPE),
            Err(EventParseError::BadKey)
        );
        let many = format!(
            r#"{{"events":[{}]}}"#,
            vec!["{}"; MAX_B2_EVENTS + 1].join(",")
        );
        assert_eq!(
            parse_b2_events(many.as_bytes(), SCOPE),
            Err(EventParseError::TooManyRecords)
        );
        let huge = vec![b' '; MAX_EVENT_MESSAGE_BYTES + 1];
        assert_eq!(
            parse_b2_events(&huge, SCOPE),
            Err(EventParseError::TooLarge)
        );
    }

    #[test]
    fn signing_secrets_are_32_letters_or_digits_and_never_printed() {
        let secret = B2SigningSecret::parse("MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM0").expect("secret");
        assert!(!format!("{secret:?}").contains("MTIz"));
        assert!(B2SigningSecret::parse("short").is_err());
        assert!(B2SigningSecret::parse("MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM-").is_err());
    }

    #[test]
    fn own_rules_are_recognised_replaced_and_removed() {
        let secret = B2SigningSecret::parse("MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM0").expect("secret");
        let url = "https://library.example/hooks/t/changes";
        let names = rule_names("library/");
        assert!(names.iter().all(|name| name.len() <= 63));
        assert_ne!(names, rule_names(""));
        let other = json!({"name": "theirs", "eventTypes": ["b2:ObjectCreated:*"],
            "isEnabled": true, "objectNamePrefix": "x/", "isSuspended": false,
            "targetConfiguration": {"targetType": "webhook", "url": "https://x.example/"}});
        let mut listed: Vec<Value> = own_rules("library/", url, &secret);
        listed.push(other.clone());
        let reply = serde_json::to_vec(&json!({"bucketId": "b", "eventNotificationRules": listed}))
            .expect("json");
        let rules = parse_notification_rules(&reply).expect("rules");
        assert!(own_rules_in_place(&rules, "library/", url, &secret));
        assert!(!own_rules_in_place(
            &rules,
            "library/",
            "https://moved.example/",
            &secret
        ));
        assert!(!own_rules_in_place(&rules, "", url, &secret));

        let kept = rules_with(&rules, "library/", Vec::new());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["name"], "theirs");
        assert!(
            kept[0].get("isSuspended").is_none(),
            "computed fields are not sent"
        );
        assert_eq!(
            rules_with(&rules, "library/", own_rules("library/", url, &secret)).len(),
            4
        );

        let mut suspended = rules.clone();
        suspended[0].is_suspended = true;
        assert!(!own_rules_in_place(&suspended, "library/", url, &secret));
    }

    #[test]
    fn unreadable_rule_replies_are_errors() {
        for reply in [
            "[]",
            "{}",
            r#"{"eventNotificationRules":[1]}"#,
            r#"{"eventNotificationRules":[{}]}"#,
            r#"{"eventNotificationRules":[{"name":"a"},{"name":"a"}]}"#,
        ] {
            assert!(
                parse_notification_rules(reply.as_bytes()).is_err(),
                "{reply}"
            );
        }
        assert!(
            parse_notification_rules(b"{\"eventNotificationRules\":[],\"x\":\"\xff\"}").is_err()
        );
        let ok = parse_notification_rules(br#"{"eventNotificationRules":[{"name":"a","targetConfiguration":{"hmacSha256SigningSecret":"bad"}}]}"#)
            .expect("rules");
        assert!(ok[0].signing_secret.is_none() && !ok[0].is_enabled);
    }
}
