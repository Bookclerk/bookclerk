//! `b64:` bind encoding shared by generic SQL executors.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

/// Encode bytes as a `b64:`-prefixed JSON string.
#[must_use]
pub fn bytes_to_b64_string(bytes: &[u8]) -> String {
    format!("b64:{}", BASE64.encode(bytes))
}

/// Decode a `b64:`-prefixed string. Returns `None` if not prefixed.
#[must_use]
pub fn b64_string_to_bytes(s: &str) -> Option<Vec<u8>> {
    s.strip_prefix("b64:").and_then(|b| BASE64.decode(b).ok())
}
