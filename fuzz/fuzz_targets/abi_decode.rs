//! Coverage-guided decoding of guest-produced Cap'n Proto messages
//! (`bookclerk-plugin-abi`): every generated `read_*` codec through
//! `read_any_message`, plus the hand-written database envelope decoders.
//!
//! The first input byte selects the decoder; the rest is the unpacked
//! single-segment message. Each decoder runs with the traversal budget the
//! host uses, so the oracle is "no panic and no unbounded allocation" on
//! arbitrary bytes a plugin could return. A decoded `err` member also runs
//! through the `limit_exceeded` parsers (`PluginError::limit`,
//! `LimitExceeded::from_message`), which the host applies to every guest
//! error message, and a parsed record must format back to a message that
//! parses to itself.

#![no_main]

use bookclerk_plugin_abi::{
    decode_db_capabilities_reply_bytes, decode_db_value_bytes, decode_execute_request_bytes,
    decode_execute_result_reply_bytes, read_any_message, LimitExceeded, PluginError,
    READ_ANY_COUNT,
};
use libfuzzer_sys::fuzz_target;

/// Hand-written decoders ahead of the generated table.
const HAND_WRITTEN: u8 = 4;

/// Largest message worth decoding; the wire caps replies far below this.
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    match selector % (HAND_WRITTEN + READ_ANY_COUNT) {
        0 => {
            let _ = decode_execute_request_bytes(bytes);
        }
        1 => {
            if let Err(err) = decode_execute_result_reply_bytes(bytes) {
                check_limit_parsers(&err);
            }
            check_limit_message(&String::from_utf8_lossy(bytes));
        }
        2 => {
            let _ = decode_db_value_bytes(bytes);
        }
        3 => {
            if let Err(err) = decode_db_capabilities_reply_bytes(bytes) {
                check_limit_parsers(&err);
            }
        }
        generated => {
            let _ = read_any_message(generated - HAND_WRITTEN, bytes);
        }
    }
});

/// Runs the host-side limit parsers over a guest-produced error.
fn check_limit_parsers(err: &PluginError) {
    if let Some(limit) = err.limit() {
        assert_eq!(
            LimitExceeded::from_message(&limit.to_message()).as_ref(),
            Some(&limit),
            "a parsed limit must round-trip through its canonical message"
        );
        assert_eq!(LimitExceeded::from_details(&limit.details()).as_ref(), Some(&limit));
    }
    check_limit_message(&err.message);
}

/// Parses arbitrary text as a limit message; a hit must round-trip.
fn check_limit_message(message: &str) {
    if let Some(limit) = LimitExceeded::from_message(message) {
        assert_eq!(
            LimitExceeded::from_message(&limit.to_message()),
            Some(limit),
            "a parsed limit must round-trip through its canonical message"
        );
    }
}
