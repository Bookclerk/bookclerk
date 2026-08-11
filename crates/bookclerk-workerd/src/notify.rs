//! HOST.notify reverse-channel HTTP parsing and event buffering.
//!
//! Pure helpers so auth / Content-Length / buffer-cap rules can be unit-tested
//! without spawning workerd.

use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Hard max for notify request bodies (bytes).
pub const NOTIFY_MAX_BODY: usize = 65536;
/// Cap on in-memory reverse-channel events (drop-oldest when exceeded).
pub const NOTIFY_EVENT_CAP: usize = 256;
/// Max concurrent notify accept handlers.
pub const NOTIFY_ACCEPT_LIMIT: usize = 8;

/// Constant-time equality for bearer tokens.
///
/// Always walks `expected` (second argument) once and folds length mismatch into
/// the same accumulator, so unauthorized inputs of any length take work
/// proportional to the expected token rather than returning early.
#[must_use]
pub fn constant_time_eq(provided: &str, expected: &str) -> bool {
    let provided = provided.as_bytes();
    let expected = expected.as_bytes();
    // `u8` is enough: any non-zero length delta collapses to a non-zero bit.
    let mut diff = u8::from(provided.len() != expected.len());
    for (i, &eb) in expected.iter().enumerate() {
        let pb = provided.get(i).copied().unwrap_or(0);
        diff |= pb ^ eb;
    }
    diff == 0
}

/// Extract `Bearer <token>` from an Authorization header value.
#[must_use]
pub fn parse_bearer(authorization: Option<&str>) -> Option<&str> {
    let value = authorization?.trim();
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Parse HTTP headers from the header block (no request line).
/// Returns lowercased header name → raw value (first occurrence wins).
pub fn parse_header_map(header_block: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in header_block.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        out.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    out
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Parse and authorize a notify HTTP request.
///
/// Requires:
/// - `POST …/notify`
/// - `Authorization: Bearer <token>` matching `expected_token` (constant-time)
/// - Valid `Content-Length` in `0..=NOTIFY_MAX_BODY`
/// - Body byte length matching Content-Length (after optional trailing junk is truncated)
///
/// Returns `(event, body_len)`.
pub fn parse_notify_http(raw: &str, expected_token: &str) -> Result<(Value, usize)> {
    let (header_part, body_part) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))
        .context("incomplete HTTP request")?;
    let request_line = header_part
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches('\r');
    if !request_line.starts_with("POST ") {
        bail!("expected POST");
    }
    // Path may be absolute (`/notify`) or absolute-form URL.
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    if !path.contains("/notify") {
        bail!("expected /notify path");
    }

    let headers = parse_header_map(header_part);
    let provided = parse_bearer(header(&headers, "authorization"));
    match provided {
        Some(token) if constant_time_eq(token, expected_token) => {}
        _ => bail!("unauthorized"),
    }

    let content_length = header(&headers, "content-length").context("missing Content-Length")?;
    let content_length: usize = content_length.parse().context("invalid Content-Length")?;
    if content_length > NOTIFY_MAX_BODY {
        bail!("body exceeds max {NOTIFY_MAX_BODY}");
    }

    // Body may include trailing bytes from a larger read; honor Content-Length.
    let body_bytes = body_part.as_bytes();
    if body_bytes.len() < content_length {
        bail!("incomplete body");
    }
    let body = std::str::from_utf8(&body_bytes[..content_length]).context("body utf-8")?;
    let body = body.trim_start_matches('\u{feff}');

    let event = if content_length == 0 || body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body.trim()).context("notify JSON body")?
    };
    Ok((event, content_length))
}

/// Event type string for logging (object `.type` only); never the full body.
#[must_use]
pub fn event_type_for_log(event: &Value) -> Option<&str> {
    event.as_object()?.get("type")?.as_str()
}

/// Push an event, dropping the oldest when at capacity. Returns `true` if dropped.
pub fn push_notify_event(events: &Mutex<Vec<Value>>, event: Value) -> Result<bool> {
    let mut guard = events
        .lock()
        .map_err(|_| anyhow::anyhow!("notify event mutex poisoned"))?;
    let mut dropped = false;
    while guard.len() >= NOTIFY_EVENT_CAP {
        guard.remove(0);
        dropped = true;
    }
    guard.push(event);
    Ok(dropped)
}

/// Generate a ~32-byte hex bearer token for one isolate.
#[must_use]
pub fn generate_bridge_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_request(auth: Option<&str>, content_length: Option<&str>, body: &str) -> String {
        let mut headers = String::from("POST /notify HTTP/1.1\r\nHost: host-notify\r\n");
        if let Some(a) = auth {
            headers.push_str(&format!("Authorization: {a}\r\n"));
        }
        if let Some(cl) = content_length {
            headers.push_str(&format!("Content-Length: {cl}\r\n"));
        }
        headers.push_str("Content-Type: application/json\r\n\r\n");
        headers.push_str(body);
        headers
    }

    #[test]
    fn rejects_missing_authorization() {
        let raw = raw_request(None, Some("2"), "{}");
        let err = parse_notify_http(&raw, "secret").unwrap_err().to_string();
        assert!(err.contains("unauthorized"), "{err}");
    }

    #[test]
    fn rejects_wrong_token() {
        let raw = raw_request(Some("Bearer wrong"), Some("2"), "{}");
        let err = parse_notify_http(&raw, "secret").unwrap_err().to_string();
        assert!(err.contains("unauthorized"), "{err}");
    }

    #[test]
    fn rejects_missing_content_length() {
        let raw = raw_request(Some("Bearer secret"), None, "{}");
        let err = parse_notify_http(&raw, "secret").unwrap_err().to_string();
        assert!(err.contains("Content-Length"), "{err}");
    }

    #[test]
    fn rejects_oversized_content_length() {
        let too_big = (NOTIFY_MAX_BODY + 1).to_string();
        let raw = raw_request(Some("Bearer secret"), Some(&too_big), "");
        let err = parse_notify_http(&raw, "secret").unwrap_err().to_string();
        assert!(err.contains("exceeds max"), "{err}");
    }

    #[test]
    fn accepts_valid_notify() {
        let body = r#"{"type":"ping","n":1}"#;
        let raw = raw_request(Some("Bearer secret"), Some(&body.len().to_string()), body);
        let (event, size) = parse_notify_http(&raw, "secret").unwrap();
        assert_eq!(size, body.len());
        assert_eq!(event["type"], "ping");
    }

    #[test]
    fn event_buffer_drops_oldest() {
        let events = Mutex::new(Vec::new());
        for i in 0..(NOTIFY_EVENT_CAP + 3) {
            let dropped = push_notify_event(&events, serde_json::json!(i)).unwrap();
            if i < NOTIFY_EVENT_CAP {
                assert!(!dropped);
            } else {
                assert!(dropped);
            }
        }
        let guard = events.lock().unwrap();
        assert_eq!(guard.len(), NOTIFY_EVENT_CAP);
        assert_eq!(guard[0], serde_json::json!(3));
        assert_eq!(
            guard[NOTIFY_EVENT_CAP - 1],
            serde_json::json!(NOTIFY_EVENT_CAP + 2)
        );
    }

    #[test]
    fn constant_time_eq_length_mismatch() {
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("ab", "abc"));
        assert!(!constant_time_eq("", "tok"));
        assert!(!constant_time_eq("longer-than-expected", "tok"));
        assert!(constant_time_eq("tok", "tok"));
        assert!(!constant_time_eq("toK", "tok"));
    }

    #[test]
    fn event_type_only_from_object() {
        assert_eq!(
            event_type_for_log(&serde_json::json!({"type": "x"})),
            Some("x")
        );
        assert_eq!(event_type_for_log(&Value::Null), None);
        assert_eq!(event_type_for_log(&serde_json::json!([1])), None);
    }
}
