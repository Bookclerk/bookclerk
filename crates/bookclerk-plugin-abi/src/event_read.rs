//! Validation of one `EventPublisher.read` request and its opaque cursor.
//!
//! The request comes from a jailed guest: over Cap'n Proto for native guests
//! and as JSON on the granted `/events/read` route for workerd isolates. The
//! host checks it here, sans-IO, before it touches the outbox. The
//! `event_read` fuzz target drives these functions (`fuzz/oracles/event_read.rs`)
//! with the same code the host runs.
//!
//! A cursor names the last outbox row a page scanned: its stored `created_at`
//! text and its id. On the wire it is `v1.<hex created_at>.<hex id>` in
//! lowercase hex, which guests treat as opaque. Only the canonical spelling
//! decodes, so a cursor that decodes always re-encodes to the same text.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::limits::{MAX_EVENT_CURSOR_BYTES, MAX_LIST_PAGE};
use crate::{DomainEvent, PluginError};

/// Parses the JSON body of one `/events/read` request (the workerd route's
/// form of [`EventReadRequest`]): a JSON object with only the request's
/// camelCase fields, each optional.
///
/// # Arguments
///
/// * `body` - Request body bytes from the isolate (untrusted).
///
/// # Errors
///
/// Returns `invalid_params` when `body` is not such an object. An array is
/// refused even though serde could read the struct from one.
pub fn parse_event_read_json(body: &[u8]) -> Result<EventReadRequest, PluginError> {
    let malformed = |err: serde_json::Error| {
        PluginError::invalid_params(format!("malformed event read: {err}"))
    };
    match serde_json::from_slice(body).map_err(malformed)? {
        value @ serde_json::Value::Object(_) => serde_json::from_value(value).map_err(malformed),
        _ => Err(PluginError::invalid_params(
            "malformed event read: expected a JSON object",
        )),
    }
}

/// Version prefix of the cursor wire form.
const CURSOR_PREFIX: &str = "v1.";

/// Longest stored `created_at` text a cursor may carry, in bytes.
pub const MAX_CURSOR_CREATED_AT_BYTES: usize = 64;

/// Longest outbox event id a cursor may carry, in bytes.
pub const MAX_CURSOR_EVENT_ID_BYTES: usize = 128;

/// Request of `EventPublisher.read`: one page of past outbox events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventReadRequest {
    /// Opaque cursor from an earlier [`EventReadPage::cursor`]; empty starts
    /// at [`Self::since_unix_ms`].
    #[serde(default)]
    pub after: String,
    /// UTC Unix milliseconds to start from when `after` is empty; zero starts
    /// at the oldest retained event.
    #[serde(default)]
    pub since_unix_ms: u64,
    /// Event types to return; empty means every consented subscription.
    #[serde(default)]
    pub types: Vec<String>,
    /// Most events to return; zero means `maxListPage`.
    #[serde(default)]
    pub limit: u32,
}

/// Success payload of `EventPublisher.read`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventReadPage {
    /// Matching events in outbox order. `delivery_attempt` is zero and
    /// `deduplication_key` is the event id.
    pub events: Vec<DomainEvent>,
    /// Opaque position after the last event scanned; empty only when nothing
    /// was scanned and the request had no cursor.
    pub cursor: String,
    /// True when more events may follow.
    pub more: bool,
    /// Ids of the events on this page whose delivery to this plugin expired
    /// unleased.
    #[serde(default)]
    pub expired_event_ids: Vec<String>,
    /// True when the start position is older than the events retention
    /// window, so earlier events may be gone.
    #[serde(default)]
    pub truncated: bool,
}

/// Position of the last outbox row a read scanned.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventCursor {
    /// The row's stored `created_at` text (RFC 3339), compared as stored.
    created_at: String,
    /// The row's event id, the tie-breaker inside one `created_at`.
    id: String,
}

impl EventCursor {
    /// Builds a cursor for one outbox row.
    ///
    /// # Arguments
    ///
    /// * `created_at` - The row's stored `created_at` text.
    /// * `id` - The row's event id.
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` when `created_at` is not RFC 3339 or either
    /// part is empty, too long, or contains whitespace or control characters.
    pub fn new(created_at: impl Into<String>, id: impl Into<String>) -> Result<Self, PluginError> {
        let created_at = created_at.into();
        let id = id.into();
        if !is_cursor_part(&created_at, MAX_CURSOR_CREATED_AT_BYTES)
            || DateTime::parse_from_rfc3339(&created_at).is_err()
        {
            return Err(PluginError::invalid_cursor(
                "cursor time is not an RFC 3339 timestamp",
            ));
        }
        if !is_cursor_part(&id, MAX_CURSOR_EVENT_ID_BYTES) {
            return Err(PluginError::invalid_cursor("cursor event id is malformed"));
        }
        Ok(Self { created_at, id })
    }

    /// The stored `created_at` text of the row.
    #[must_use]
    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    /// The event id of the row.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Encodes the cursor as its opaque wire text.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{CURSOR_PREFIX}{}.{}",
            hex::encode(self.created_at.as_bytes()),
            hex::encode(self.id.as_bytes())
        )
    }

    /// Decodes the opaque wire text a guest sent back.
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` when `text` is longer than
    /// `maxEventCursorBytes`, is not the canonical `v1.<hex>.<hex>` form, or
    /// names a row [`Self::new`] would refuse.
    pub fn decode(text: &str) -> Result<Self, PluginError> {
        let malformed = || PluginError::invalid_cursor("malformed event cursor");
        if text.len() > MAX_EVENT_CURSOR_BYTES as usize {
            return Err(PluginError::invalid_cursor(format!(
                "event cursor of {} bytes exceeds {MAX_EVENT_CURSOR_BYTES}",
                text.len()
            )));
        }
        let rest = text.strip_prefix(CURSOR_PREFIX).ok_or_else(malformed)?;
        let (time_hex, id_hex) = rest.split_once('.').ok_or_else(malformed)?;
        let part = |hex_text: &str| -> Result<String, PluginError> {
            if !hex_text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(malformed());
            }
            let bytes = hex::decode(hex_text).map_err(|_| malformed())?;
            String::from_utf8(bytes).map_err(|_| malformed())
        };
        Self::new(part(time_hex)?, part(id_hex)?)
    }
}

/// True when `part` is non-empty, at most `max` bytes, and holds no
/// whitespace or control characters.
fn is_cursor_part(part: &str, max: usize) -> bool {
    !part.is_empty()
        && part.len() <= max
        && part.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

/// Where a validated read starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventReadStart {
    /// The oldest retained event.
    Oldest,
    /// The first event stored at or after this time.
    Since(DateTime<Utc>),
    /// The first event after this cursor.
    After(EventCursor),
}

/// A read request the host accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReadQuery {
    /// Where the read starts.
    pub start: EventReadStart,
    /// Consented event types to return; never empty.
    pub types: BTreeSet<String>,
    /// Most events to return, `1..=maxListPage`.
    pub limit: u32,
}

/// Checks one guest read request against the bounds `plugin.capnp` declares
/// and the instance's consented subscription types.
///
/// # Arguments
///
/// * `request` - The guest's request.
/// * `consented` - Event types the instance's consented `[[events.consumers]]`
///   rows subscribe to.
///
/// # Returns
///
/// The validated query, with an empty `types` list widened to `consented`
/// and a zero `limit` raised to `maxListPage`.
///
/// # Errors
///
/// * `forbidden` when the instance consumes no events, or a requested type
///   is not one it consumes.
/// * `invalid_cursor` when `after` does not decode.
/// * `invalid_params` when both `after` and `sinceUnixMs` are set, when
///   `sinceUnixMs` is not a representable time, or when `limit` or the type
///   list exceeds `maxListPage`.
pub fn validate_event_read_request(
    request: &EventReadRequest,
    consented: &BTreeSet<String>,
) -> Result<EventReadQuery, PluginError> {
    if consented.is_empty() {
        return Err(PluginError::forbidden(
            "reading events needs a consented [[events.consumers]] row",
        ));
    }
    let max = MAX_LIST_PAGE;
    if request.limit > max {
        return Err(PluginError::invalid_params(format!(
            "limit {} exceeds {max}",
            request.limit
        )));
    }
    if request.types.len() > max as usize {
        return Err(PluginError::invalid_params(format!(
            "{} event types exceed {max}",
            request.types.len()
        )));
    }
    let start = match (request.after.is_empty(), request.since_unix_ms) {
        (true, 0) => EventReadStart::Oldest,
        (true, ms) => {
            let since = i64::try_from(ms)
                .ok()
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .ok_or_else(|| PluginError::invalid_params("sinceUnixMs is out of range"))?;
            EventReadStart::Since(since)
        }
        (false, 0) => EventReadStart::After(EventCursor::decode(&request.after)?),
        (false, _) => {
            return Err(PluginError::invalid_params(
                "set at most one of after and sinceUnixMs",
            ))
        }
    };
    let types = if request.types.is_empty() {
        consented.clone()
    } else {
        let mut types = BTreeSet::new();
        for event_type in &request.types {
            if !consented.contains(event_type) {
                return Err(PluginError::forbidden(format!(
                    "`{event_type}` is not a consented [[events.consumers]] type"
                )));
            }
            types.insert(event_type.clone());
        }
        types
    };
    let limit = if request.limit == 0 {
        max
    } else {
        request.limit
    };
    Ok(EventReadQuery {
        start,
        types,
        limit,
    })
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    fn consented() -> BTreeSet<String> {
        ["book_acquired", "title_added"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    #[test]
    fn a_cursor_round_trips_through_its_wire_text() {
        let cursor = EventCursor::new("2026-11-11T10:00:00.123+00:00", "evt-1").unwrap();
        let text = cursor.encode();
        assert!(text.starts_with("v1."));
        assert_eq!(EventCursor::decode(&text).unwrap(), cursor);
        assert_eq!(cursor.created_at(), "2026-11-11T10:00:00.123+00:00");
        assert_eq!(cursor.id(), "evt-1");
    }

    #[test]
    fn only_the_canonical_cursor_spelling_decodes() {
        let text = EventCursor::new("2026-11-11T10:00:00Z", "a")
            .unwrap()
            .encode();
        for bad in [
            text.to_uppercase(),
            text.replacen("v1.", "v2.", 1),
            format!("{text}."),
            text.replace('.', ""),
            "v1.zz.61".to_string(),
            "v1.6.61".to_string(),
            String::new(),
            "x".repeat(MAX_EVENT_CURSOR_BYTES as usize + 1),
        ] {
            assert_eq!(
                EventCursor::decode(&bad).unwrap_err().wire_str(),
                "invalid_cursor",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_cursor_refuses_rows_it_cannot_name() {
        for (time, id) in [
            ("yesterday", "a"),
            ("2026-11-11T10:00:00Z", ""),
            ("2026-11-11T10:00:00Z", "a b"),
            ("2026-11-11T10:00:00Z", "a\u{7}"),
            (" 2026-11-11T10:00:00Z", "a"),
        ] {
            assert!(EventCursor::new(time, id).is_err(), "{time:?} {id:?}");
        }
        let long_id = "a".repeat(MAX_CURSOR_EVENT_ID_BYTES + 1);
        assert!(EventCursor::new("2026-11-11T10:00:00Z", long_id).is_err());
    }

    #[test]
    fn an_empty_request_reads_every_consented_type_from_the_oldest() {
        let query =
            validate_event_read_request(&EventReadRequest::default(), &consented()).unwrap();
        assert_eq!(query.start, EventReadStart::Oldest);
        assert_eq!(query.types, consented());
        assert_eq!(query.limit, MAX_LIST_PAGE);
    }

    #[test]
    fn a_request_starts_after_its_cursor_or_at_its_time() {
        let cursor = EventCursor::new("2026-11-11T10:00:00Z", "evt").unwrap();
        let after = EventReadRequest {
            after: cursor.encode(),
            types: vec!["title_added".into(), "title_added".into()],
            limit: 5,
            ..EventReadRequest::default()
        };
        let query = validate_event_read_request(&after, &consented()).unwrap();
        assert_eq!(query.start, EventReadStart::After(cursor));
        assert_eq!(query.types.len(), 1);
        assert_eq!(query.limit, 5);
        let since = EventReadRequest {
            since_unix_ms: 1_700_000_000_000,
            ..EventReadRequest::default()
        };
        assert_eq!(
            validate_event_read_request(&since, &consented())
                .unwrap()
                .start,
            EventReadStart::Since(DateTime::from_timestamp_millis(1_700_000_000_000).unwrap())
        );
    }

    #[test]
    fn a_request_outside_the_rules_is_refused() {
        let code = |request: EventReadRequest, consented: &BTreeSet<String>| {
            validate_event_read_request(&request, consented)
                .unwrap_err()
                .wire_str()
                .to_string()
        };
        assert_eq!(
            code(EventReadRequest::default(), &BTreeSet::new()),
            "forbidden"
        );
        let other = EventReadRequest {
            types: vec!["secret_type".into()],
            ..EventReadRequest::default()
        };
        assert_eq!(code(other, &consented()), "forbidden");
        let both = EventReadRequest {
            after: EventCursor::new("2026-11-11T10:00:00Z", "e")
                .unwrap()
                .encode(),
            since_unix_ms: 1,
            ..EventReadRequest::default()
        };
        assert_eq!(code(both, &consented()), "invalid_params");
        let far = EventReadRequest {
            since_unix_ms: u64::MAX,
            ..EventReadRequest::default()
        };
        assert_eq!(code(far, &consented()), "invalid_params");
        let big = EventReadRequest {
            limit: MAX_LIST_PAGE + 1,
            ..EventReadRequest::default()
        };
        assert_eq!(code(big, &consented()), "invalid_params");
        let many = EventReadRequest {
            types: vec!["book_acquired".into(); MAX_LIST_PAGE as usize + 1],
            ..EventReadRequest::default()
        };
        assert_eq!(code(many, &consented()), "invalid_params");
        let bad = EventReadRequest {
            after: "v1.zz".into(),
            ..EventReadRequest::default()
        };
        assert_eq!(code(bad, &consented()), "invalid_cursor");
    }

    #[test]
    fn the_request_json_rejects_unknown_fields() {
        let ok: EventReadRequest = serde_json::from_str(r#"{"limit":3}"#).unwrap();
        assert_eq!(ok.limit, 3);
        assert!(serde_json::from_str::<EventReadRequest>(r#"{"account":"x"}"#).is_err());
    }
}
