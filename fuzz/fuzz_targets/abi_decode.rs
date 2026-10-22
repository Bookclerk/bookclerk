//! Coverage-guided decoding of guest-produced Cap'n Proto messages
//! (`bookclerk-plugin-abi`): every generated `read_*` codec through
//! `read_any_message`, plus the hand-written database envelope decoders.
//!
//! The first input byte selects the decoder; the rest is the unpacked
//! single-segment message. Each decoder runs with the traversal budget the
//! host uses, so the oracle is "no panic and no unbounded allocation" on
//! arbitrary bytes a plugin could return.

#![no_main]

use bookclerk_plugin_abi::{
    decode_db_value_bytes, decode_execute_request_bytes, decode_execute_result_reply_bytes,
    read_any_message, READ_ANY_COUNT,
};
use libfuzzer_sys::fuzz_target;

/// Hand-written decoders ahead of the generated table.
const HAND_WRITTEN: u8 = 3;

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
            let _ = decode_execute_result_reply_bytes(bytes);
        }
        2 => {
            let _ = decode_db_value_bytes(bytes);
        }
        generated => {
            let _ = read_any_message(generated - HAND_WRITTEN, bytes);
        }
    }
});
