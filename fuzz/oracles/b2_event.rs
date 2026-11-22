//! Shared oracle for the `b2_event` fuzz target and the `bookclerk-storage`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is what the `b2` destination's change feed reads from B2:
//! byte 0 picks the boundary and the scope, and the rest is the bytes.
//!
//! - Even byte 0: one webhook body of event notifications. Bit 1 picks the
//!   destination's key prefix (clear: none, set: `library/`).
//! - Odd byte 0: a `b2_get_bucket_notification_rules` (or set) reply.
//!
//! The invariants:
//!
//! - parsing never panics and gives the same answer twice;
//! - an input over its size cap is refused as too large;
//! - when the event parser accepts a body, an independent reading of the
//!   same JSON (a `serde_json::Value` walk) finds exactly the same keys, in
//!   order, each a non-empty `changed` entry without details, and the same
//!   times; and when that reading finds a well-formed body, the parser
//!   accepts it;
//! - when the rules parser accepts a reply, the independent reading finds
//!   the same rule names in order, all distinct and within B2's length,
//!   at most the cap, and every signing secret it reports is 32 letters or
//!   digits; sending back the rules that are not the destination's own
//!   keeps each one as listed, without B2's computed fields.

use bookclerk_storage::b2_api::check_file_name;
use bookclerk_storage::b2_events::{
    parse_b2_events, parse_notification_rules, rule_names, rules_with, B2EventScope, MAX_B2_EVENTS,
    MAX_NOTIFICATION_RULES,
};
use bookclerk_storage::s3_events::{EventParseError, MAX_EVENT_MESSAGE_BYTES};
use bookclerk_storage::ObjectChangeKind;
use serde_json::Value;

/// The bucket every input is checked against.
const BUCKET: &str = "books";

/// Checks one input; panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    if selector % 2 == 0 {
        let prefix = if selector & 2 == 0 { "" } else { "library/" };
        check_events(rest, prefix);
    } else {
        check_rules(rest);
    }
}

/// The oracle's own reading of a webhook body: `Some` keys and times when
/// the body is well formed, `None` when it should be refused.
fn expected_events(body: &[u8], prefix: &str) -> Option<Vec<(String, Option<String>)>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let events = value.get("events")?.as_array()?;
    if !value.is_object() || events.len() > MAX_B2_EVENTS {
        return None;
    }
    let mut out = Vec::new();
    for event in events {
        let event = event.as_object()?;
        let event_type = event.get("eventType")?.as_str()?;
        let parts: Vec<&str> = event_type.splitn(3, ':').collect();
        let reported = parts.len() == 3
            && parts[0] == "b2"
            && !parts[2].is_empty()
            && ["ObjectCreated", "ObjectDeleted", "HideMarkerCreated"].contains(&parts[1]);
        // Only a reported event needs its bucket (a test event may lack one).
        if !reported || event.get("bucketName")?.as_str()? != BUCKET {
            continue;
        }
        let name = event.get("objectName")?.as_str()?;
        let Some(key) = name.strip_prefix(prefix) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        if check_file_name(name).is_err() {
            return None;
        }
        let time = match event.get("eventTimestamp") {
            None | Some(Value::Null) => None,
            Some(stamp) => Some(format!("{:020}", stamp.as_u64()?)),
        };
        out.push((key.to_string(), time));
    }
    Some(out)
}

/// Checks the webhook-body boundary.
fn check_events(body: &[u8], prefix: &str) {
    let scope = B2EventScope {
        bucket: BUCKET,
        prefix,
    };
    let first = parse_b2_events(body, scope);
    assert_eq!(first, parse_b2_events(body, scope));
    if body.len() > MAX_EVENT_MESSAGE_BYTES {
        assert_eq!(first, Err(EventParseError::TooLarge));
        return;
    }
    let expected = expected_events(body, prefix);
    match first {
        Ok(events) => {
            let found: Vec<(String, Option<String>)> = events
                .into_iter()
                .map(|event| {
                    assert_eq!(event.change.kind, ObjectChangeKind::Changed);
                    assert!(event.change.info.is_none());
                    assert!(event.sequencer.is_none());
                    assert!(!event.change.key.is_empty());
                    assert!(!event.change.key.contains('\0'));
                    (event.change.key, event.time)
                })
                .collect();
            assert_eq!(Some(found), expected, "the parser and the oracle disagree");
        }
        Err(_) => assert!(expected.is_none(), "a well-formed body was refused"),
    }
}

/// Checks the rules-reply boundary.
fn check_rules(reply: &[u8]) {
    let first = parse_notification_rules(reply);
    assert_eq!(first, parse_notification_rules(reply));
    let Ok(rules) = first else {
        return;
    };
    let value: Value = serde_json::from_slice(reply).expect("accepted reply is JSON");
    let listed = value["eventNotificationRules"]
        .as_array()
        .expect("accepted reply lists rules");
    assert!(rules.len() <= MAX_NOTIFICATION_RULES);
    assert_eq!(rules.len(), listed.len());
    for (rule, raw) in rules.iter().zip(listed) {
        assert_eq!(Some(rule.name.as_str()), raw["name"].as_str());
        assert!(!rule.name.is_empty() && rule.name.len() <= 63);
        if let Some(secret) = &rule.signing_secret {
            let text = secret.expose();
            assert_eq!(text.len(), 32);
            assert!(text.bytes().all(|b| b.is_ascii_alphanumeric()));
        }
    }
    for (at, rule) in rules.iter().enumerate() {
        assert!(rules[..at].iter().all(|seen| seen.name != rule.name));
    }
    let own = rule_names("library/");
    let kept = rules_with(&rules, "library/", Vec::new());
    let others: Vec<&Value> = listed
        .iter()
        .filter(|raw| {
            !own.iter()
                .any(|name| raw["name"].as_str() == Some(name.as_str()))
        })
        .collect();
    assert_eq!(kept.len(), others.len());
    for (sent, raw) in kept.iter().zip(others) {
        let mut raw = raw.as_object().expect("rule object").clone();
        raw.remove("isSuspended");
        raw.remove("suspensionReason");
        assert_eq!(sent, &Value::Object(raw));
    }
}
