//! Coverage-guided codec identity for the generated Cap'n Proto DTOs
//! (`bookclerk-plugin-abi`): every value a `read_*` codec accepts must come
//! back equal after `write_*` and `read_*` again.
//!
//! The first input byte selects the root struct, the rest is the unpacked
//! single-segment message, as in `abi_decode`. Bytes the decoder rejects are
//! not findings; a decoded value that re-encodes differently, fails to
//! re-decode, or compares unequal is a defect in a generated codec pair
//! (`docs/adr/untrusted-input-fuzzing.md`: for codecs, `read(write(v)) == v`).

#![no_main]

use bookclerk_plugin_abi::{roundtrip_any_message, RoundTripFailure, READ_ANY_COUNT};
use libfuzzer_sys::fuzz_target;

/// Largest message worth decoding; the wire caps replies far below this.
const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    match roundtrip_any_message(selector % READ_ANY_COUNT, bytes) {
        Ok(()) | Err(RoundTripFailure::Rejected(_)) => {}
        Err(RoundTripFailure::Codec(why)) => panic!("selector {selector}: {why}"),
    }
});
