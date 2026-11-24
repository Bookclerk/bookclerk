//! Shared oracle for the `notifier_reply` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! Byte 0 modulo 3 picks what the rest is:
//!
//! - **0**: an unpacked `NotifySendReply` message, a notifier's answer to
//!   `Notifier.send`. [`classify_send_reply`] must deliver exactly the `ok`
//!   arm, keep a provider id exactly when it is a non-empty line within the
//!   field cap, retry exactly `rate_limited`, `unavailable`, `deadline_exceeded` and `cancelled` (waiting only
//!   on `rate_limited`, never past an hour), and cut every reason to one line
//!   of at most 512 bytes that is a prefix of the full text with control
//!   characters blanked. The reply round-trips through the codec.
//! - **1**: an address document as UTF-8 text, what a notifier's
//!   `validateDestination` answer or a patron's form carries.
//!   [`parse_address_document`] must accept it exactly when an independent
//!   restatement of the document rules does, and an accepted document must
//!   re-serialise to text that parses back equal. [`check_address_against_fields`]
//!   must accept it against a fixed `email` / `name` field list exactly when
//!   it has a non-blank `email` and no other key than those two.
//! - **2**: an unpacked `NotifyDeliverabilityReply` message, a notifier's
//!   answer to `checkDeliverability`. [`check_deliverability_report`] must
//!   refuse exactly a report over 8 checks or 32 pieces of advice, keep the
//!   order and count of every list, cut each text to the longest prefix
//!   within its cap with control and layout characters blanked, keep a
//!   report link exactly when an independent restatement of the `https`
//!   rules accepts it, show `skipped` advice as `warning`, and keep at most
//!   8 sources. The reply round-trips through the codec.

use std::time::Duration;

use bookclerk_plugin_abi::notifications::{
    check_address_against_fields, check_deliverability_report, classify_send_reply,
    decode_notify_deliverability_reply_bytes, decode_notify_send_reply_bytes,
    encode_notify_deliverability_reply_bytes, encode_notify_send_reply_bytes,
    parse_address_document, AddressFieldRule, SendOutcome,
};
use bookclerk_plugin_abi::{NotifyCheckOutcome, PluginErrorCode};

/// Largest input worth decoding; the RPC caps a reply far below this.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// Field value cap in bytes (`maxNotificationFieldBytes`).
const FIELD: usize = 1_024;
/// Title cap in bytes (`maxNotificationTitleBytes`).
const TITLE: usize = 256;
/// Most checks in a deliverability report (`maxDeliverabilityChecks`).
const CHECKS: usize = 8;
/// Most pieces of advice in a report (`maxDeliverabilityAdvice`).
const ADVICE: usize = 32;
/// Address document cap in bytes (`maxNotifierAddressBytes`).
const ADDRESS: usize = 4_096;
/// Delivery-log reason cap in bytes.
const REASON: usize = 512;
/// Longest wait the host honours.
const HOUR: Duration = Duration::from_secs(3_600);

/// Characters that break a line or reorder text without being control
/// characters, restated as a list: line and paragraph separators, the
/// Arabic letter mark, LRM / RLM, the embeddings and overrides, and the
/// isolates.
fn layout(c: char) -> bool {
    const LIST: [u32; 14] = [
        0x2028, 0x2029, 0x061C, 0x200E, 0x200F, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066,
        0x2067, 0x2068, 0x2069,
    ];
    LIST.contains(&u32::from(c))
}

/// Runs the invariants on one input.
///
/// # Panics
///
/// When an invariant breaks.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 3 {
        0 => check_send_reply(rest),
        1 => check_address(rest),
        _ => check_deliverability_reply(rest),
    }
}

/// The send-reply invariants.
fn check_send_reply(bytes: &[u8]) {
    let Ok(reply) = decode_notify_send_reply_bytes(bytes) else {
        return;
    };
    let outcome = classify_send_reply(&reply);
    match (&reply, &outcome) {
        (Ok(sent), SendOutcome::Delivered { provider_id }) => {
            let usable = sent.provider_id.as_deref().filter(|id| {
                !id.is_empty()
                    && id.len() <= FIELD
                    && id.chars().all(|c| !c.is_control() && !layout(c))
            });
            assert_eq!(provider_id.as_deref(), usable, "provider id kept wrongly");
        }
        (Err(err), SendOutcome::Retry { after, reason }) => {
            assert!(
                matches!(
                    err.code,
                    PluginErrorCode::RateLimited
                        | PluginErrorCode::Unavailable
                        | PluginErrorCode::DeadlineExceeded
                        | PluginErrorCode::Cancelled
                ),
                "retried a permanent error"
            );
            if err.code != PluginErrorCode::RateLimited {
                assert!(after.is_none(), "waited without a rate limit");
            }
            let asked = err
                .details
                .as_ref()
                .and_then(|d| d.get("retryAfterMs"))
                .and_then(serde_json::Value::as_u64)
                .map(Duration::from_millis);
            if err.code == PluginErrorCode::RateLimited {
                assert_eq!(*after, asked.map(|a| a.min(HOUR)), "wrong wait");
            }
            check_reason(reason, &format!("{}: {}", err.code, err.message));
        }
        (Err(err), SendOutcome::Failed { reason }) => {
            assert!(
                !matches!(
                    err.code,
                    PluginErrorCode::RateLimited
                        | PluginErrorCode::Unavailable
                        | PluginErrorCode::DeadlineExceeded
                        | PluginErrorCode::Cancelled
                ),
                "gave up on a retryable error"
            );
            check_reason(reason, &format!("{}: {}", err.code, err.message));
        }
        (Ok(_), _) => panic!("an ok reply was not delivered"),
        (Err(_), SendOutcome::Delivered { .. }) => panic!("an error was delivered"),
    }
    let encoded = encode_notify_send_reply_bytes(&reply)
        .unwrap_or_else(|err| panic!("re-encode a decoded reply: {err}"));
    let again = decode_notify_send_reply_bytes(&encoded)
        .unwrap_or_else(|err| panic!("decode a re-encoded reply: {err}"));
    assert_eq!(again, reply, "send reply does not round-trip");
}

/// A reason is the full text with control characters blanked, cut to at
/// most [`REASON`] bytes on a character boundary, and cut only when the
/// next character would not fit.
fn check_reason(reason: &str, full: &str) {
    let blanked: String = full
        .chars()
        .map(|c| if c.is_control() || layout(c) { ' ' } else { c })
        .collect();
    assert!(reason.len() <= REASON, "reason too long");
    assert!(blanked.starts_with(reason), "reason is not a prefix");
    if reason.len() < blanked.len() {
        let next = blanked[reason.len()..]
            .chars()
            .next()
            .map_or(0, char::len_utf8);
        assert!(reason.len() + next > REASON, "reason cut early");
    }
}

/// A key is `[a-z][a-z0-9_]{0,63}`.
fn key_ok(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && key.len() <= 64
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// Keys of the top-level object in source order (duplicates kept), or
/// `None` when `text` is not one JSON object.
fn raw_keys(text: &str) -> Option<Vec<(String, serde_json::Value)>> {
    struct Keys;
    impl<'de> serde::de::Visitor<'de> for Keys {
        type Value = Vec<(String, serde_json::Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(entry) = map.next_entry::<String, serde_json::Value>()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let keys = serde::Deserializer::deserialize_map(&mut de, Keys).ok()?;
    de.end().ok()?;
    Some(keys)
}

/// The address-document invariants.
fn check_address(bytes: &[u8]) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let parsed = parse_address_document(text);
    let expected = text.len() <= ADDRESS
        && raw_keys(text).is_some_and(|entries| {
            let mut seen = std::collections::BTreeSet::new();
            entries.len() <= 16
                && entries.iter().all(|(key, value)| {
                    seen.insert(key.clone())
                        && key_ok(key)
                        && value
                            .as_str()
                            .is_some_and(|v| v.len() <= FIELD && v.chars().all(|c| !c.is_control()))
                })
        });
    assert_eq!(parsed.is_ok(), expected, "address document {text:?}");
    let Ok(address) = parsed else {
        return;
    };
    let again = parse_address_document(&address.to_json())
        .unwrap_or_else(|err| panic!("canonical text parses: {err}"));
    assert_eq!(again, address, "address does not round-trip");
    let rules = [
        AddressFieldRule {
            key: "email".into(),
            required: true,
        },
        AddressFieldRule {
            key: "name".into(),
            required: false,
        },
    ];
    let fits = address.keys().all(|k| k == "email" || k == "name")
        && address.get("email").is_some_and(|v| !v.trim().is_empty());
    assert_eq!(
        check_address_against_fields(&address, &rules).is_ok(),
        fits,
        "address fields"
    );
}

/// `text` with control and layout characters blanked, cut to the longest
/// prefix of at most `cap` bytes.
fn cut(text: &str, cap: usize) -> String {
    let blanked: String = text
        .chars()
        .map(|c| if c.is_control() || layout(c) { ' ' } else { c })
        .collect();
    let mut end = blanked.len().min(cap);
    while !blanked.is_char_boundary(end) {
        end -= 1;
    }
    blanked[..end].to_owned()
}

/// A report link the operator may follow: `https://`, a host without
/// credentials, within the field cap, no space, control, layout character
/// or backslash anywhere.
fn report_url_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..host_end];
    url.len() <= FIELD
        && !authority.is_empty()
        && !authority.contains('@')
        && url
            .chars()
            .all(|c| !(c.is_whitespace() || c.is_control() || layout(c) || c == '\\'))
}

/// The deliverability-reply invariants.
fn check_deliverability_reply(bytes: &[u8]) {
    let Ok(reply) = decode_notify_deliverability_reply_bytes(bytes) else {
        return;
    };
    let encoded = encode_notify_deliverability_reply_bytes(&reply)
        .unwrap_or_else(|err| panic!("re-encode a decoded report: {err}"));
    let again = decode_notify_deliverability_reply_bytes(&encoded)
        .unwrap_or_else(|err| panic!("decode a re-encoded report: {err}"));
    assert_eq!(again, reply, "deliverability reply does not round-trip");
    let Ok(report) = reply else {
        return;
    };
    let over = report.checks.len() > CHECKS || report.advice.len() > ADVICE;
    let checked = check_deliverability_report(report.clone());
    assert_eq!(checked.is_err(), over, "refused the wrong reports");
    let Ok(shown) = checked else {
        return;
    };
    assert_eq!(shown.checks.len(), report.checks.len(), "checks dropped");
    for (out, inp) in shown.checks.iter().zip(&report.checks) {
        assert_eq!(out.service, cut(&inp.service, TITLE), "service");
        assert_eq!(out.summary, cut(&inp.summary, FIELD), "summary");
        assert_eq!(out.outcome, inp.outcome, "check outcome");
        let kept = inp.report_url.clone().filter(|url| report_url_ok(url));
        assert_eq!(out.report_url, kept, "report link {:?}", inp.report_url);
    }
    assert_eq!(shown.advice.len(), report.advice.len(), "advice dropped");
    for (out, inp) in shown.advice.iter().zip(&report.advice) {
        let outcome = if inp.outcome == NotifyCheckOutcome::Skipped {
            NotifyCheckOutcome::Warning
        } else {
            inp.outcome
        };
        assert_eq!(out.outcome, outcome, "advice outcome");
        assert_ne!(out.outcome, NotifyCheckOutcome::Skipped, "skipped advice");
        assert_eq!(out.title, cut(&inp.title, TITLE), "title");
        assert_eq!(out.detail, cut(&inp.detail, FIELD), "detail");
        let sources: Vec<String> = inp
            .sources
            .iter()
            .take(CHECKS)
            .map(|source| cut(source, TITLE))
            .collect();
        assert_eq!(out.sources, sources, "sources");
    }
}
