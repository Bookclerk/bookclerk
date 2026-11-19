//! Shared oracle for the `tags_wire` fuzz target and the `bookclerk-tags`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The tag worker decodes request frames from the host, and the host
//! decodes reply frames from the worker, which parses untrusted files and so
//! is treated as compromised. The input's first byte picks the decoder
//! (even: request, odd: reply), the rest is the frame body. The invariants:
//! a request or a `Need`/`Failed` reply that decodes re-encodes to exactly
//! the same bytes, so a frame has one reading; an accepted `Need` names one
//! to `MAX_SPANS_PER_ROUND` non-empty spans that do not overflow; an
//! accepted `Failed` message is within `MAX_ERROR_BYTES`; and an accepted
//! `Done` record re-encodes to a frame that decodes to the same record.

use bookclerk_tags::wire::{
    decode_reply, decode_request, encode_reply, encode_request, Reply, MAX_ERROR_BYTES,
};
use bookclerk_tags::{MAX_FETCH_BYTES, MAX_SPANS_PER_ROUND};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - Selector byte, then the frame body.
///
/// # Panics
///
/// When an invariant above does not hold.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    if selector % 2 == 0 {
        if let Ok(request) = decode_request(body) {
            assert_eq!(
                encode_request(&request).expect("re-encode").get(4..),
                Some(body)
            );
        }
        return;
    }
    let Ok(reply) = decode_reply(body) else {
        return;
    };
    let frame = encode_reply(&reply).expect("re-encode");
    match &reply {
        Reply::Need { spans, .. } => {
            assert!(!spans.is_empty() && spans.len() <= MAX_SPANS_PER_ROUND);
            for span in spans {
                assert!(span.len > 0 && span.len <= MAX_FETCH_BYTES);
                assert!(span.end().is_some(), "overflowing span accepted");
            }
            assert_eq!(frame.get(4..), Some(body));
        }
        Reply::Failed { message, .. } => {
            assert!(message.len() <= MAX_ERROR_BYTES, "long message accepted");
            assert_eq!(frame.get(4..), Some(body));
        }
        Reply::Done { .. } => {
            assert_eq!(decode_reply(&frame[4..]).expect("decode again"), reply);
        }
    }
}
