//! Shared oracle for the `node_frame` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the text of one node-to-node frame, as a peer node sends it
//! over the cluster socket. The invariants:
//!
//! - parsing never panics, and refuses text over the frame size limit or of
//!   another frame version;
//! - an accepted frame whose serialized text stays within the limit
//!   parses back to an equal frame (a float inside an opaque JSON field may
//!   move by a rounding step, as `serde_json` parses floats without exact
//!   round trips);
//! - every library id an `acquire.targets` frame carries is 1 to 16 ASCII
//!   letters and digits, checked byte by byte here rather than through the
//!   parser's own rule, since a host names storage keys with it.

use bookclerk_node_proto::{Body, Frame, FRAME_VERSION, MAX_FRAME_BYTES, MAX_LIBRARY_ID_BYTES};
use serde_json::Value;

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(frame) = Frame::parse(text) else {
        return;
    };
    assert!(text.len() <= MAX_FRAME_BYTES, "an oversized frame parsed");
    assert_eq!(frame.v, FRAME_VERSION, "another frame version parsed");
    let text_again = frame.to_text();
    // Serializing can lengthen an opaque JSON field (`1e9` is written out
    // in full), past the limit the parser enforces: such a frame is never
    // sent again, so there is no round trip to check.
    if text_again.len() > MAX_FRAME_BYTES {
        return;
    }
    let again = Frame::parse(&text_again);
    let Ok(again) = again else {
        panic!("a parsed frame did not parse again: {again:?}");
    };
    assert!(
        same(&json(&again), &json(&frame)),
        "a frame changed across a round trip: {frame:?} became {again:?}"
    );
    if let Body::AcquireTargets { targets, .. } = &frame.body {
        for target in targets {
            for id in [&target.library_id, &target.parent_library_id]
                .into_iter()
                .flatten()
            {
                let id = id.as_str();
                assert!(
                    !id.is_empty()
                        && id.len() <= MAX_LIBRARY_ID_BYTES
                        && id.bytes().all(|b| b.is_ascii_alphanumeric()),
                    "library id {id:?} accepted"
                );
            }
        }
    }
}

/// `frame` as a JSON tree.
fn json(frame: &Frame) -> Value {
    serde_json::from_str(&frame.to_text()).unwrap_or(Value::Null)
}

/// True when `a` and `b` are the same tree, floats equal to within a few
/// rounding steps.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    x == y || (x - y).abs() <= x.abs().max(y.abs()) * 4.0 * f64::EPSILON
                }
                _ => false,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, x)| y.get(key).is_some_and(|y| same(x, y)))
        }
        _ => a == b,
    }
}
