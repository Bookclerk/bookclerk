//! Shared oracle for the `event_read` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one `EVENTS.read` message. Byte 0 modulo 4 selects the
//! form: 0 is the JSON body of the workerd `/events/read` route, 1 is a bare
//! cursor (lossy UTF-8), 2 is a Cap'n Proto `EventReadRequest` and 3 a Cap'n
//! Proto `EventReadPage` (unpacked, single segment). The invariants:
//!
//! - parsing, decoding and validation never panic;
//! - [`parse_event_read_json`] accepts exactly the JSON objects whose keys
//!   are request fields, and an accepted request round-trips through JSON;
//! - [`validate_event_read_request`] accepts the request, against a fixed
//!   consent, exactly when the independent restatement below does, with
//!   the same error code and the same resolved start, types and limit;
//! - [`EventCursor::decode`] accepts exactly the cursors the independent
//!   decoder below accepts, and an accepted cursor re-encodes to the same
//!   text;
//! - a Cap'n Proto request or page the RPC codec accepts survives
//!   `write` then `read` unchanged and stays inside the wire bounds: a
//!   cursor of at most `maxEventCursorBytes` and lists of at most
//!   `maxListPage` entries.

use std::collections::BTreeSet;

use bookclerk_plugin_abi::{
    parse_event_read_json, roundtrip_event_read_message, validate_event_read_request, EventCursor,
    EventReadCodec, EventReadMessage, EventReadRequest, EventReadStart, PluginErrorCode,
    RoundTripFailure, MAX_CURSOR_CREATED_AT_BYTES, MAX_CURSOR_EVENT_ID_BYTES,
    MAX_EVENT_CURSOR_BYTES, MAX_LIST_PAGE,
};
use chrono::{DateTime, Utc};

/// Largest input worth parsing; a real request is under 64 KiB.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// Event types the fuzzed plugin is consented to read.
const CONSENTED: [&str; 2] = ["book_acquired", "title_added"];

/// Field names the JSON form may carry.
const REQUEST_FIELDS: [&str; 4] = ["after", "sinceUnixMs", "types", "limit"];

/// One lowercase-hex cursor part decoded by hand; `None` when malformed.
fn hex_part(text: &str, max: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        out.push(nibble(pair[0])? << 4 | nibble(pair[1])?);
    }
    let part = String::from_utf8(out).ok()?;
    let clean = part.len() <= max && part.chars().all(|c| !c.is_whitespace() && !c.is_control());
    clean.then_some(part)
}

/// The cursor rules restated: `(created_at, id)` when `text` is canonical.
fn expected_cursor(text: &str) -> Option<(String, String)> {
    if text.len() > MAX_EVENT_CURSOR_BYTES as usize {
        return None;
    }
    let rest = text.strip_prefix("v1.")?;
    let dot = rest.find('.')?;
    let created_at = hex_part(&rest[..dot], MAX_CURSOR_CREATED_AT_BYTES)?;
    let id = hex_part(&rest[dot + 1..], MAX_CURSOR_EVENT_ID_BYTES)?;
    DateTime::parse_from_rfc3339(&created_at).ok()?;
    Some((created_at, id))
}

/// What validation should resolve a request to, restated from the rules.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    /// Refused with this code.
    Refused(PluginErrorCode),
    /// Accepted: start (`None` oldest, `Some(Ok(ms))` since, `Some(Err(..))`
    /// after a cursor), types, limit.
    Accepted(Option<Result<i64, (String, String)>>, BTreeSet<String>, u32),
}

/// The request rules restated directly.
fn expected_validation(request: &EventReadRequest, consented: &BTreeSet<String>) -> Expected {
    let max = MAX_LIST_PAGE;
    if request.limit > max || request.types.len() > max as usize {
        return Expected::Refused(PluginErrorCode::InvalidParams);
    }
    let start = if !request.after.is_empty() {
        if request.since_unix_ms != 0 {
            return Expected::Refused(PluginErrorCode::InvalidParams);
        }
        match expected_cursor(&request.after) {
            Some(parts) => Some(Err(parts)),
            None => return Expected::Refused(PluginErrorCode::InvalidCursor),
        }
    } else if request.since_unix_ms == 0 {
        None
    } else {
        let latest = DateTime::<Utc>::MAX_UTC.timestamp_millis();
        match i64::try_from(request.since_unix_ms) {
            Ok(ms) if ms <= latest => Some(Ok(ms)),
            _ => return Expected::Refused(PluginErrorCode::InvalidParams),
        }
    };
    if request.types.iter().any(|t| !consented.contains(t)) {
        return Expected::Refused(PluginErrorCode::Forbidden);
    }
    let types = if request.types.is_empty() {
        consented.clone()
    } else {
        request.types.iter().cloned().collect()
    };
    let limit = if request.limit == 0 {
        max
    } else {
        request.limit
    };
    Expected::Accepted(start, types, limit)
}

/// Checks validation of one parsed request against the restatement.
// Oracle: a failed expectation is an invariant violation the fuzzer reports.
#[allow(clippy::expect_used)]
fn check_request(request: &EventReadRequest) {
    let consented: BTreeSet<String> = CONSENTED.iter().map(|t| (*t).to_string()).collect();
    let expected = expected_validation(request, &consented);
    let actual = match validate_event_read_request(request, &consented) {
        Err(err) => Expected::Refused(err.code),
        Ok(query) => {
            let start = match query.start {
                EventReadStart::Oldest => None,
                EventReadStart::Since(at) => Some(Ok(at.timestamp_millis())),
                EventReadStart::After(cursor) => {
                    assert_eq!(cursor.encode(), request.after, "cursor re-encodes");
                    Some(Err((
                        cursor.created_at().to_string(),
                        cursor.id().to_string(),
                    )))
                }
            };
            Expected::Accepted(start, query.types, query.limit)
        }
    };
    assert_eq!(actual, expected, "validation disagrees for {request:?}");
    let nobody =
        validate_event_read_request(request, &BTreeSet::new()).expect_err("no consent never reads");
    assert_eq!(nobody.code, PluginErrorCode::Forbidden);
}

/// Checks the JSON form of one request body.
// Oracle: a failed expectation is an invariant violation the fuzzer reports.
#[allow(clippy::expect_used)]
fn check_json(body: &[u8]) {
    let shape_ok = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(map)) => {
            map.keys().all(|key| REQUEST_FIELDS.contains(&key.as_str()))
        }
        _ => false,
    };
    match parse_event_read_json(body) {
        Ok(request) => {
            assert!(shape_ok, "accepted a body that is not a request object");
            let text = serde_json::to_vec(&request).expect("request serializes");
            let again = parse_event_read_json(&text).expect("serialized request parses");
            assert_eq!(again, request, "request round-trips through JSON");
            check_request(&request);
        }
        Err(err) => assert_eq!(err.code, PluginErrorCode::InvalidParams),
    }
}

/// Checks one bare cursor.
// Oracle: a failed expectation is an invariant violation the fuzzer reports.
#[allow(clippy::expect_used)]
fn check_cursor(text: &str) {
    let expected = expected_cursor(text);
    match EventCursor::decode(text) {
        Ok(cursor) => {
            assert_eq!(cursor.encode(), text, "accepted cursors are canonical");
            assert_eq!(
                Some((cursor.created_at().to_string(), cursor.id().to_string())),
                expected,
                "decoder accepted what the restatement refuses"
            );
            let rebuilt = EventCursor::new(cursor.created_at(), cursor.id()).expect("rebuild");
            assert_eq!(rebuilt, cursor);
        }
        Err(err) => {
            assert_eq!(err.code, PluginErrorCode::InvalidCursor);
            assert!(
                expected.is_none(),
                "decoder refused a canonical cursor: {text:?}"
            );
        }
    }
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    match selector % 4 {
        0 => check_json(rest),
        1 => check_cursor(&String::from_utf8_lossy(rest)),
        2 => check_capnp(EventReadCodec::Request, rest),
        _ => check_capnp(EventReadCodec::Page, rest),
    }
}

/// Cap'n Proto codec invariant: identity on accepted values, inside the
/// wire bounds.
fn check_capnp(codec: EventReadCodec, bytes: &[u8]) {
    let cursor_cap = MAX_EVENT_CURSOR_BYTES as usize;
    let list_cap = MAX_LIST_PAGE as usize;
    match roundtrip_event_read_message(codec, bytes) {
        Err(RoundTripFailure::Rejected(_)) => {}
        Err(RoundTripFailure::Codec(why)) => panic!("event read codec: {why}"),
        Ok(EventReadMessage::Request(request)) => {
            assert_eq!(codec, EventReadCodec::Request);
            assert!(request.after.len() <= cursor_cap, "request cursor bound");
            assert!(request.types.len() <= list_cap, "request type list bound");
        }
        Ok(EventReadMessage::Page(page)) => {
            assert_eq!(codec, EventReadCodec::Page);
            assert!(page.cursor.len() <= cursor_cap, "page cursor bound");
            assert!(page.events.len() <= list_cap, "page event list bound");
            assert!(
                page.expired_event_ids.len() <= list_cap,
                "page expired id list bound"
            );
        }
    }
}
